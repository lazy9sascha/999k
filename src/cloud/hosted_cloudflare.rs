use std::io;
use std::time::Duration;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use reqwest::{Client, Method, RequestBuilder, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::json;
use uuid::Uuid;

const CF_API: &str = "https://api.cloudflare.com/client/v4/";
const PAGE_SIZE: usize = 100;
const RESPONSE_LIMIT: usize = 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) enum ResourceState {
    #[default]
    New,
    Attempted,
    Known(String),
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct CloudflareResources {
    pub(crate) tunnel: ResourceState,
    pub(crate) dns_record: ResourceState,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ProvisionError {
    #[error("invalid Cloudflare provisioning settings")]
    Settings,
    #[error("Cloudflare registration resources do not match")]
    Conflict,
    #[error("Cloudflare create outcome is unknown")]
    Uncertain,
    #[error("failed to save Cloudflare registration state")]
    Persist,
    #[error("Cloudflare API returned HTTP {0}")]
    Http(u16),
    #[error("Cloudflare API could not be reached")]
    Transport,
    #[error("Cloudflare API returned an invalid response")]
    Response,
}

impl ProvisionError {
    fn rejected_create(&self) -> bool {
        matches!(self, Self::Http(400 | 401 | 403 | 404 | 429))
    }
}

pub(crate) struct ProvisionedTunnel {
    pub(crate) origin: String,
    connector_token: String,
}

impl ProvisionedTunnel {
    pub(crate) fn into_connector_token(self) -> String {
        self.connector_token
    }
}

pub(crate) struct CloudflareProvisioner {
    client: Client,
    api: Url,
    authorization: HeaderValue,
    account_id: String,
    zone_id: String,
    zone_name: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    success: bool,
    result: T,
    result_info: Option<Page>,
}

#[derive(Deserialize)]
struct Page {
    per_page: Option<usize>,
}

#[derive(Deserialize)]
struct Tunnel {
    id: String,
    account_tag: String,
    name: String,
    config_src: String,
}

#[derive(Deserialize)]
struct DnsRecord {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    name: String,
    content: Option<String>,
    proxied: Option<bool>,
    comment: Option<String>,
}

impl DnsRecord {
    fn matches(&self, hostname: &str, target: &str, marker: &str) -> bool {
        valid_provider_id(&self.id)
            && self.kind == "CNAME"
            && self.name == hostname
            && self.content.as_deref().map(|x| x.trim_end_matches('.')) == Some(target)
            && self.proxied == Some(true)
            && self.comment.as_deref() == Some(marker)
    }
}

fn valid_provider_id(value: &str) -> bool {
    value.len() == 32 && value.bytes().all(|c| c.is_ascii_hexdigit())
}

fn valid_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn save(
    resources: &mut CloudflareResources,
    next: CloudflareResources,
    persist: &mut impl FnMut(&CloudflareResources) -> io::Result<()>,
) -> Result<(), ProvisionError> {
    persist(&next).map_err(|_| ProvisionError::Persist)?;
    *resources = next;
    Ok(())
}

impl CloudflareProvisioner {
    pub(crate) fn new(
        account_id: &str,
        zone_id: &str,
        zone_name: &str,
        management_token: &str,
    ) -> Result<Self, ProvisionError> {
        if !valid_provider_id(account_id)
            || !valid_provider_id(zone_id)
            || zone_name.len() > 189
            || !zone_name.contains('.')
            || !zone_name.split('.').all(valid_label)
            || management_token.is_empty()
            || management_token.bytes().any(|c| c.is_ascii_whitespace())
        {
            return Err(ProvisionError::Settings);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {management_token}"))
            .map_err(|_| ProvisionError::Settings)?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| ProvisionError::Settings)?;
        Ok(Self {
            client,
            api: Url::parse(CF_API).map_err(|_| ProvisionError::Settings)?,
            authorization,
            account_id: account_id.to_owned(),
            zone_id: zone_id.to_owned(),
            zone_name: zone_name.to_owned(),
        })
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, ProvisionError> {
        let url = self.api.join(path).map_err(|_| ProvisionError::Settings)?;
        Ok(self
            .client
            .request(method, url)
            .header(AUTHORIZATION, self.authorization.clone()))
    }

    async fn send<T: DeserializeOwned>(
        request: RequestBuilder,
    ) -> Result<Envelope<T>, ProvisionError> {
        let mut response = request
            .send()
            .await
            .map_err(|_| ProvisionError::Transport)?;
        if !response.status().is_success() {
            return Err(ProvisionError::Http(response.status().as_u16()));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ProvisionError::Transport)?
        {
            if chunk.len() > RESPONSE_LIMIT - body.len() {
                return Err(ProvisionError::Response);
            }
            body.extend_from_slice(&chunk);
        }
        let envelope: Envelope<T> =
            serde_json::from_slice(&body).map_err(|_| ProvisionError::Response)?;
        if !envelope.success {
            return Err(ProvisionError::Response);
        }
        Ok(envelope)
    }

    async fn list<T: DeserializeOwned>(request: RequestBuilder) -> Result<Vec<T>, ProvisionError> {
        let page: Envelope<Vec<T>> =
            Self::send(request.query(&[("page", 1), ("per_page", PAGE_SIZE)])).await?;
        // Cloudflare's total_count includes records outside the filter. A full
        // page does not prove the lookup is complete, so require a short page.
        let page_size = page
            .result_info
            .and_then(|p| p.per_page)
            .filter(|size| *size > 0)
            .unwrap_or(PAGE_SIZE);
        if page.result.len() >= page_size {
            return Err(ProvisionError::Uncertain);
        }
        Ok(page.result)
    }

    async fn write<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, ProvisionError> {
        Ok(Self::send(
            self.request(method, path)?
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_string()),
        )
        .await?
        .result)
    }

    fn owned_tunnel(&self, tunnel: &Tunnel, name: &str) -> bool {
        tunnel.account_tag == self.account_id
            && tunnel.name == name
            && tunnel.config_src == "cloudflare"
            && Uuid::parse_str(&tunnel.id)
                .is_ok_and(|id| !id.is_nil() && id.to_string() == tunnel.id)
    }

    async fn create<T: DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
        resources: &mut CloudflareResources,
        state: fn(&mut CloudflareResources) -> &mut ResourceState,
        persist: &mut impl FnMut(&CloudflareResources) -> io::Result<()>,
    ) -> Result<T, ProvisionError> {
        // Persist the attempt before the POST. A transport failure can hide success.
        let mut next = resources.clone();
        *state(&mut next) = ResourceState::Attempted;
        save(resources, next, persist)?;

        let result = self.write(Method::POST, path, body).await;
        if result.as_ref().is_err_and(ProvisionError::rejected_create) {
            let mut next = resources.clone();
            *state(&mut next) = ResourceState::New;
            save(resources, next, persist)?;
        }
        result
    }

    async fn ensure_tunnel(
        &self,
        collection: &str,
        name: &str,
        resources: &mut CloudflareResources,
        persist: &mut impl FnMut(&CloudflareResources) -> io::Result<()>,
    ) -> Result<String, ProvisionError> {
        let tunnels = Self::list::<Tunnel>(
            self.request(Method::GET, collection)?
                .query(&[("name", name), ("is_deleted", "false")]),
        )
        .await?;
        let mut matches = tunnels.iter().filter(|tunnel| tunnel.name == name);
        let found = matches.next();
        if matches.next().is_some() {
            return Err(ProvisionError::Conflict);
        }
        let id = match (&resources.tunnel, found) {
            (ResourceState::Known(id), Some(t)) if *id == t.id && self.owned_tunnel(t, name) => {
                return Ok(id.clone());
            }
            (ResourceState::Attempted, Some(t)) if self.owned_tunnel(t, name) => t.id.clone(),
            (ResourceState::Attempted, None) => return Err(ProvisionError::Uncertain),
            (ResourceState::New, None) => {
                let tunnel: Tunnel = self
                    .create(
                        collection,
                        json!({"name": name, "config_src": "cloudflare"}),
                        resources,
                        |resources| &mut resources.tunnel,
                        persist,
                    )
                    .await?;
                if !self.owned_tunnel(&tunnel, name) {
                    return Err(ProvisionError::Uncertain);
                }
                tunnel.id
            }
            _ => return Err(ProvisionError::Conflict),
        };
        save(
            resources,
            CloudflareResources {
                tunnel: ResourceState::Known(id.clone()),
                ..resources.clone()
            },
            persist,
        )?;
        Ok(id)
    }

    /// The operator must hold the installation lock through this request and its state writes.
    /// The host, port, and MCP path come from the operator, and the installation UUID is server-issued.
    pub(crate) async fn provision(
        &self,
        installation_id: Uuid,
        hostname: &str,
        loopback_port: u16,
        approved_mcp_path: &str,
        resources: &mut CloudflareResources,
        mut persist: impl FnMut(&CloudflareResources) -> io::Result<()>,
    ) -> Result<ProvisionedTunnel, ProvisionError> {
        let first_label = hostname
            .strip_suffix(&self.zone_name)
            .and_then(|label| label.strip_suffix('.'));
        if installation_id.is_nil()
            || !first_label.is_some_and(valid_label)
            || loopback_port == 0
            || !crate::mailbox_catalog::valid_mcp_route(approved_mcp_path)
            || matches!(
                approved_mcp_path,
                "/receiver/mcp" | "/admin/mcp" | "/rpc/mcp"
            )
        {
            return Err(ProvisionError::Settings);
        }

        let collection = format!("accounts/{}/cfd_tunnel", self.account_id);
        let dns_collection = format!("zones/{}/dns_records", self.zone_id);
        let name = format!("pigeonmq-{installation_id}");
        let marker = format!("pigeonmq installation {installation_id}");
        let records = Self::list::<DnsRecord>(
            self.request(Method::GET, &dns_collection)?
                .query(&[("name.exact", hostname)]),
        )
        .await?;
        let record = match (&resources.dns_record, records.as_slice()) {
            (ResourceState::New, []) => None,
            (ResourceState::Attempted, []) => return Err(ProvisionError::Uncertain),
            (ResourceState::Attempted, [record]) if record.comment.as_deref() == Some(&marker) => {
                Some(record)
            }
            (ResourceState::Known(id), [record])
                if *id == record.id && record.comment.as_deref() == Some(&marker) =>
            {
                Some(record)
            }
            _ => return Err(ProvisionError::Conflict),
        };
        // DNS publication follows tunnel ownership and configuration. If the DNS record exists,
        // the saved tunnel ID must also exist. Never create a second tunnel for it.
        if record.is_some() && !matches!(resources.tunnel, ResourceState::Known(_)) {
            return Err(ProvisionError::Conflict);
        }
        let tunnel = self
            .ensure_tunnel(&collection, &name, resources, &mut persist)
            .await?;
        let target = format!("{tunnel}.cfargotunnel.com");
        if record.is_some_and(|r| !r.matches(hostname, &target, &marker)) {
            return Err(ProvisionError::Conflict);
        }

        let origin_host = format!("127.0.0.1:{loopback_port}");
        // valid_mcp_route restricts the path to literal regex characters.
        self.write::<serde_json::Value>(
            Method::PUT,
            &format!("{collection}/{tunnel}/configurations"),
            json!({"config":{"ingress":[
                {"hostname": hostname, "path": format!("^{approved_mcp_path}$"),
                 "service": format!("http://{origin_host}"),
                 "originRequest":{"httpHostHeader": origin_host}},
                {"service":"http_status:404"}
            ]}}),
        )
        .await?;

        let record_id = match record {
            Some(record) => record.id.clone(),
            None => {
                let record: DnsRecord = self
                    .create(
                        &dns_collection,
                        json!({"type":"CNAME","name":hostname,"content":target,
                               "proxied":true,"ttl":1,"comment":marker}),
                        resources,
                        |resources| &mut resources.dns_record,
                        &mut persist,
                    )
                    .await?;
                if !record.matches(hostname, &target, &marker) {
                    return Err(ProvisionError::Uncertain);
                }
                record.id
            }
        };
        if resources.dns_record != ResourceState::Known(record_id.clone()) {
            save(
                resources,
                CloudflareResources {
                    dns_record: ResourceState::Known(record_id),
                    ..resources.clone()
                },
                &mut persist,
            )?;
        }

        let token: Envelope<String> =
            Self::send(self.request(Method::GET, &format!("{collection}/{tunnel}/token"))?).await?;
        if token.result.is_empty() || token.result.bytes().any(|c| c.is_ascii_whitespace()) {
            return Err(ProvisionError::Response);
        }
        Ok(ProvisionedTunnel {
            origin: format!("https://{hostname}"),
            connector_token: token.result,
        })
    }
}

#[cfg(test)]
#[path = "hosted_cloudflare_tests.rs"]
mod tests;
