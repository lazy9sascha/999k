use super::*;
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConnectionHistory {
    NeverConnected,
    PreviouslyConnected,
    Unknown,
}

pub(crate) struct SavedCloudflareInstallation<'a> {
    pub(crate) installation_id: Uuid,
    pub(crate) account_id: &'a str,
    pub(crate) zone_id: &'a str,
    pub(crate) hostname: &'a str,
    pub(crate) resources: &'a CloudflareResources,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ProviderLifecycleOutcome {
    Complete,
    Pending,
    Retained,
}

#[derive(Deserialize)]
struct TunnelDetails {
    #[serde(flatten)]
    identity: Tunnel,
    status: Option<String>,
    conns_active_at: Option<String>,
    deleted_at: Option<String>,
}

impl TunnelDetails {
    fn history(&self) -> ConnectionHistory {
        if self.conns_active_at.is_some() {
            return ConnectionHistory::PreviouslyConnected;
        }
        match self.status.as_deref() {
            Some("inactive") => ConnectionHistory::NeverConnected,
            Some("healthy" | "degraded" | "down") => ConnectionHistory::PreviouslyConnected,
            _ => ConnectionHistory::Unknown,
        }
    }
}

struct ExactTargets<'a> {
    tunnel_id: &'a str,
    dns_id: &'a str,
    tunnel_path: String,
    dns_path: String,
    tunnel_name: String,
    dns_marker: String,
    dns_target: String,
    hostname: &'a str,
}

impl CloudflareProvisioner {
    fn exact_targets<'a>(
        &self,
        saved: &SavedCloudflareInstallation<'a>,
    ) -> Result<ExactTargets<'a>, ProvisionError> {
        let hostname_label = saved
            .hostname
            .strip_suffix(&self.zone_name)
            .and_then(|label| label.strip_suffix('.'));
        if saved.account_id != self.account_id
            || saved.zone_id != self.zone_id
            || saved.installation_id.is_nil()
            || !hostname_label.is_some_and(valid_label)
        {
            return Err(ProvisionError::Conflict);
        }
        let (ResourceState::Known(tunnel_id), ResourceState::Known(dns_id)) =
            (&saved.resources.tunnel, &saved.resources.dns_record)
        else {
            return Err(ProvisionError::Conflict);
        };
        if !valid_provider_id(dns_id)
            || !Uuid::parse_str(tunnel_id)
                .is_ok_and(|id| !id.is_nil() && id.to_string() == *tunnel_id)
        {
            return Err(ProvisionError::Conflict);
        }
        Ok(ExactTargets {
            tunnel_id,
            dns_id,
            tunnel_path: format!("accounts/{}/cfd_tunnel/{tunnel_id}", self.account_id),
            dns_path: format!("zones/{}/dns_records/{dns_id}", self.zone_id),
            tunnel_name: format!("pigeonmq-{}", saved.installation_id),
            dns_marker: format!("pigeonmq installation {}", saved.installation_id),
            dns_target: format!("{tunnel_id}.cfargotunnel.com"),
            hostname: saved.hostname,
        })
    }

    async fn get_exact<T: DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<Option<T>, ProvisionError> {
        match Self::send(self.request(Method::GET, path)?).await {
            Ok(response) => Ok(Some(response.result)),
            Err(ProvisionError::Http(404)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn read_tunnel(
        &self,
        targets: &ExactTargets<'_>,
    ) -> Result<Option<TunnelDetails>, ProvisionError> {
        let Some(tunnel) = self
            .get_exact::<TunnelDetails>(&targets.tunnel_path)
            .await?
        else {
            return Ok(None);
        };
        if tunnel.identity.id != targets.tunnel_id
            || !self.owned_tunnel(&tunnel.identity, &targets.tunnel_name)
        {
            return Err(ProvisionError::Conflict);
        }
        if let Some(deleted_at) = &tunnel.deleted_at {
            chrono::DateTime::parse_from_rfc3339(deleted_at)
                .map_err(|_| ProvisionError::Response)?;
            return Ok(None);
        }
        Ok(Some(tunnel))
    }

    async fn dns_exists(&self, targets: &ExactTargets<'_>) -> Result<bool, ProvisionError> {
        let Some(dns) = self.get_exact::<DnsRecord>(&targets.dns_path).await? else {
            return Ok(false);
        };
        if dns.id != targets.dns_id
            || !dns.matches(targets.hostname, &targets.dns_target, &targets.dns_marker)
        {
            return Err(ProvisionError::Conflict);
        }
        Ok(true)
    }

    async fn delete_exact(&self, path: &str) -> Result<(), ProvisionError> {
        match Self::send::<serde_json::Value>(self.request(Method::DELETE, path)?).await {
            Ok(_) | Err(ProvisionError::Http(404)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn tunnel_connection_history(
        &self,
        saved: &SavedCloudflareInstallation<'_>,
    ) -> Result<ConnectionHistory, ProvisionError> {
        let targets = self.exact_targets(saved)?;
        Ok(self
            .read_tunnel(&targets)
            .await?
            .map_or(ConnectionHistory::Unknown, |tunnel| tunnel.history()))
    }

    /// Hold the installation lock and keep its saved IDs while revocation is pending.
    pub(crate) async fn revoke(
        &self,
        saved: &SavedCloudflareInstallation<'_>,
    ) -> Result<ProviderLifecycleOutcome, ProvisionError> {
        use ProviderLifecycleOutcome::*;
        let targets = self.exact_targets(saved)?;
        let tunnel = self.read_tunnel(&targets).await?;
        if self.dns_exists(&targets).await? {
            self.delete_exact(&targets.dns_path).await?;
            if self.dns_exists(&targets).await? {
                return Ok(Pending);
            }
        }
        if tunnel.is_some() {
            let connections = format!("{}/connections", targets.tunnel_path);
            // The tunnel status endpoint's deprecated connections array cannot prove emptiness.
            let connectors: Envelope<Vec<serde::de::IgnoredAny>> =
                Self::send(self.request(Method::GET, &connections)?).await?;
            if !connectors.result.is_empty() {
                self.delete_exact(&connections).await?;
            }
            // A plain DELETE refuses active connectors. The old token stays valid until
            // deletion, so a reconnect can keep this operation pending.
            self.delete_exact(&targets.tunnel_path).await?;
            if self.read_tunnel(&targets).await?.is_some() {
                return Ok(Pending);
            }
        }
        Ok(Complete)
    }

    /// Call after the cutoff and sticky-history checks. Save the expiry attempt and
    /// keep the exact IDs so an interrupted delete can finish removing its DNS.
    pub(crate) async fn expire_unopened(
        &self,
        saved: &SavedCloudflareInstallation<'_>,
    ) -> Result<ProviderLifecycleOutcome, ProvisionError> {
        use ProviderLifecycleOutcome::*;
        let targets = self.exact_targets(saved)?;
        let route_present = self.dns_exists(&targets).await?;
        // Read the exact tunnel after checking DNS, immediately before plain deletion.
        // Cloudflare checks for active connectors, but has no never-connected precondition.
        if let Some(tunnel) = self.read_tunnel(&targets).await? {
            match tunnel.history() {
                ConnectionHistory::NeverConnected => {}
                ConnectionHistory::PreviouslyConnected if route_present => return Ok(Retained),
                ConnectionHistory::PreviouslyConnected | ConnectionHistory::Unknown => {
                    return Err(ProvisionError::Uncertain);
                }
            }
            self.delete_exact(&targets.tunnel_path).await?;
            if self.read_tunnel(&targets).await?.is_some() {
                return Ok(Pending);
            }
        }
        if self.dns_exists(&targets).await? {
            self.delete_exact(&targets.dns_path).await?;
            if self.dns_exists(&targets).await? {
                return Ok(Pending);
            }
        }
        Ok(Complete)
    }
}
