use super::*;
use enrollment_store::{LifecyclePhase, LifecycleRecord};
use hosted_cloudflare::lifecycle::{
    ConnectionHistory, ProviderLifecycleOutcome, SavedCloudflareInstallation,
};
use serde::Serialize;
use std::fs::File;

#[derive(Deserialize, Serialize)]
struct BoundResources {
    account_id: String,
    zone_id: String,
    resources: CloudflareResources,
}

pub(super) fn bound_resources(
    account_id: &str,
    zone_id: &str,
    raw: Option<&[u8]>,
) -> io::Result<CloudflareResources> {
    let Some(raw) = raw else {
        return Ok(CloudflareResources::default());
    };
    let saved: BoundResources = serde_json::from_slice(raw).map_err(|_| unavailable())?;
    if saved.account_id != account_id || saved.zone_id != zone_id {
        return Err(unavailable());
    }
    Ok(saved.resources)
}

fn pending() -> io::Error {
    io::Error::other("registration lifecycle operation is pending; retry the same operation")
}

impl Operator {
    /// Both the private CLI and the serving process take this lock for Cloudflare writes.
    pub(super) fn provider_guard(&self) -> io::Result<File> {
        arrival_notify::lock_state(&self.home.join("cloudflare-operation.state"))
            .map_err(|_| pending())
    }

    pub(super) fn bound_resources(&self, raw: Option<&[u8]>) -> io::Result<CloudflareResources> {
        bound_resources(&self.account_id, &self.zone_id, raw)
    }

    pub(super) fn persist_resources(
        &self,
        id: Uuid,
        resources: &CloudflareResources,
    ) -> io::Result<()> {
        self.persist_route_resources(id, resources, None)
    }

    pub(super) fn persist_route_resources(
        &self,
        id: Uuid,
        resources: &CloudflareResources,
        ready_snapshot: Option<&[u8]>,
    ) -> io::Result<()> {
        let data = serde_json::to_vec(&BoundResources {
            account_id: self.account_id.clone(),
            zone_id: self.zone_id.clone(),
            resources: resources.clone(),
        })
        .map_err(|_| unavailable())?;
        if let Some(previous) = ready_snapshot {
            return self
                .record(|registry| registry.update_ready_resources(id, previous, &data))
                .map_err(|_| unavailable());
        }
        let now = epoch_seconds().map_err(|_| unavailable())?;
        self.record(|registry| registry.save_resources(id, &data, now))
            .map(|_| ())
            .map_err(|_| unavailable())
    }

    pub(super) fn check_finish_lifecycle(&self, id: Uuid) -> io::Result<()> {
        let record = self
            .record(|registry| registry.lifecycle(id))
            .map_err(|_| unavailable())?;
        match record.phase {
            LifecyclePhase::Approved | LifecyclePhase::Ready => Ok(()),
            _ => Err(pending()),
        }
    }

    fn saved<'a>(
        &'a self,
        id: Uuid,
        hostname: &'a str,
        resources: &'a CloudflareResources,
    ) -> SavedCloudflareInstallation<'a> {
        SavedCloudflareInstallation {
            installation_id: id,
            account_id: &self.account_id,
            zone_id: &self.zone_id,
            hostname,
            resources,
        }
    }

    pub(super) async fn track_dot_access(&self, id: Uuid, app_id: &str) -> io::Result<()> {
        let _guard = self.provider_guard()?;
        let record = self
            .record(|registry| registry.lifecycle(id))
            .map_err(|_| unavailable())?;
        let snapshot = record.resources.as_deref().ok_or_else(unavailable)?;
        let mut resources = self.bound_resources(Some(snapshot))?;
        if record.phase != LifecyclePhase::Ready || resources.selected_routes.is_none() {
            return Err(io::Error::other(
                "a ready address-first installation is required",
            ));
        }
        if resources
            .dot_access_app
            .as_deref()
            .is_some_and(|saved| saved != app_id)
        {
            return Err(io::Error::other(
                "this installation already tracks another Dot Access app",
            ));
        }
        let hostname = self.hostname(id);
        let cloud = self
            .cloudflare
            .inspect_owned(&self.saved(id, &hostname, &resources))
            .await
            .map_err(io::Error::other)?;
        if cloud["tunnel"] != "present" || cloud["dns_record"] != "present" {
            return Err(io::Error::other(
                "the saved tunnel and DNS record must both be present",
            ));
        }
        self.cloudflare
            .require_dot_access(&hostname, app_id)
            .await
            .map_err(io::Error::other)?;
        resources.dot_access_app = Some(app_id.to_owned());
        self.persist_route_resources(id, &resources, Some(snapshot))
    }

    pub(super) async fn revoke_installation(&self, id: Uuid) -> io::Result<()> {
        let _guard = self.provider_guard()?;
        let record = self
            .record(|registry| registry.request_revoke(id))
            .map_err(|_| unavailable())?;
        match record.phase {
            LifecyclePhase::Revoked | LifecyclePhase::Expired => Ok(()),
            LifecyclePhase::Revoking => self.resume_removal(record).await,
            _ => Err(unavailable()),
        }
    }

    pub(super) fn reissue_installation(&self, id: Uuid) -> io::Result<()> {
        let _guard = self.provider_guard()?;
        self.record(|registry| registry.request_reissue(id))
            .map(|_| ())
            .map_err(|_| unavailable())
    }

    async fn resume_removal(&self, record: LifecycleRecord) -> io::Result<()> {
        let id = record.id;
        let phase = record.phase;
        let resources = self.bound_resources(record.resources.as_deref())?;
        let hostname = self.hostname(id);
        let target = self.saved(id, &hostname, &resources);
        let result = match phase {
            LifecyclePhase::Revoking => self.cloudflare.revoke(&target).await,
            LifecyclePhase::Expiring => self.cloudflare.expire_unopened(&target).await,
            _ => return Err(unavailable()),
        }
        .map_err(|_| pending())?;
        match result {
            ProviderLifecycleOutcome::Complete => self
                .record(|registry| registry.complete_removal(id, phase))
                .map(|_| ())
                .map_err(|_| unavailable()),
            ProviderLifecycleOutcome::Retained if phase == LifecyclePhase::Expiring => self
                .record(|registry| registry.retain_connected(id, phase))
                .map(|_| ())
                .map_err(|_| unavailable()),
            _ => Err(pending()),
        }
    }

    async fn expire_if_unopened(&self, id: Uuid) -> io::Result<()> {
        let _guard = self.provider_guard()?;
        let record = self
            .record(|registry| registry.lifecycle(id))
            .map_err(|_| unavailable())?;
        match record.phase {
            LifecyclePhase::Revoking | LifecyclePhase::Expiring => {
                self.resume_removal(record).await
            }
            LifecyclePhase::Ready if !record.ever_connected => {
                let resources = self.bound_resources(record.resources.as_deref())?;
                let hostname = self.hostname(id);
                let saved = self.saved(id, &hostname, &resources);
                match self
                    .cloudflare
                    .tunnel_connection_history(&saved)
                    .await
                    .map_err(|_| pending())?
                {
                    ConnectionHistory::NeverConnected => {
                        let now = epoch_seconds().map_err(|_| unavailable())?;
                        let record = self
                            .record(|registry| registry.request_expiry(id, now))
                            .map_err(|_| pending())?;
                        self.resume_removal(record).await
                    }
                    ConnectionHistory::PreviouslyConnected => self
                        .record(|registry| registry.retain_connected(id, LifecyclePhase::Ready))
                        .map(|_| ())
                        .map_err(|_| unavailable()),
                    ConnectionHistory::Unknown => Err(pending()),
                }
            }
            _ => Ok(()),
        }
    }

    pub(super) async fn cleanup_loop(self: Arc<Self>) {
        let mut interval = tokio::time::interval(Duration::from_secs(15 * 60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            let Ok(now) = epoch_seconds() else { continue };
            let Ok(candidates) = self.record(|registry| registry.lifecycle_candidates(now)) else {
                tracing::warn!("registration lifecycle candidates unavailable");
                continue;
            };
            for record in candidates {
                if self.expire_if_unopened(record.id).await.is_err() {
                    tracing::warn!("registration lifecycle operation pending");
                }
            }
        }
    }
}
