#[path = "../cloud/enrollment_store.rs"]
mod enrollment_store;
#[path = "../cloud/hosted_cloudflare.rs"]
mod hosted_cloudflare;
#[path = "../cloud/hosted_email.rs"]
mod hosted_email;
#[path = "../cloud/hosted_registration_http.rs"]
mod hosted_registration_http;
#[path = "../cloud/hosted_registration_lifecycle.rs"]
mod hosted_registration_lifecycle;

use clap::{ArgGroup, Parser};
use enrollment_store::{Enrollment, EnrollmentState, LifecycleRecord, Registry, RegistryError};
#[path = "../cloud/hosted_origin.rs"]
mod hosted_origin;
use hosted_cloudflare::lifecycle::SavedCloudflareInstallation;
use hosted_cloudflare::{CloudflareProvisioner, CloudflareResources, valid_public_mcp_path};
use hosted_email::{SmtpMailer, SmtpSettings};
use hosted_origin::UnixOrigin;
use hosted_registration_http::{
    RegistrationHttpError as HttpError, RegistrationResponse, RequestKind, approval_result,
    json_response, login_form, otp_form, parse_request,
};
use http::{Method, Request, StatusCode, header};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::{TokioIo, TokioTimer};
use radio999k::{arrival_notify, private_file};
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;
use std::{
    convert::Infallible,
    io::{self, Read},
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{net::TcpListener, sync::Semaphore};
use uuid::Uuid;

#[derive(Parser)]
#[command(
    name = "999k-registration",
    about = "Serve and manage hosted 999K registration",
    group(ArgGroup::new("operation").args(["list", "status", "revoke", "reissue", "track_dot_access"]))
)]
struct Options {
    #[arg(long)]
    home: PathBuf,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..), required_unless_present = "operation", conflicts_with = "operation")]
    port: Option<u16>,
    /// List registrations and the recorded Cloudflare lifecycle state.
    #[arg(long)]
    list: bool,
    /// Inspect the recorded lifecycle state of one installation.
    #[arg(long)]
    status: Option<Uuid>,
    /// Read Cloudflare's current state for the installation selected by --status.
    #[arg(long, requires = "status")]
    check_cloudflare: bool,
    /// Track an existing dedicated Dot Access application for one ready installation.
    #[arg(long, requires = "access_app")]
    track_dot_access: Option<Uuid>,
    /// The exact Cloudflare Access application ID for --track-dot-access.
    #[arg(long, requires = "track_dot_access")]
    access_app: Option<String>,
    /// Revoke the public connection of one enrolled installation.
    #[arg(long)]
    revoke: Option<Uuid>,
    /// Let a revoked installation resume with a new tunnel at its assigned URL.
    #[arg(long)]
    reissue: Option<Uuid>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    registration_origin: String,
    email_smtp: Option<SmtpSettings>,
    #[serde(default, rename = "supabase_url")]
    _supabase_url: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "supabase_publishable_key")]
    _supabase_publishable_key: Option<serde::de::IgnoredAny>,
    // Read older settings without treating an obsolete list as authorization.
    #[serde(default)]
    invited_accounts: Option<serde::de::IgnoredAny>,
    cloudflare_account_id: String,
    cloudflare_zone_id: String,
    cloudflare_zone_name: String,
    hub_port: u16,
    mcp_path: Option<String>,
    #[serde(default)]
    mcp_paths: Vec<String>,
    selectable_mcp_paths: Option<Vec<String>>,
}

impl Config {
    fn read(home: &Path) -> io::Result<Self> {
        serde_json::from_slice(&read_private(&home.join("settings.json"))?)
            .map_err(|_| unavailable())
    }

    fn cloudflare(&self, home: &Path) -> io::Result<CloudflareProvisioner> {
        let token = read_private(&home.join("cloudflare.key"))?;
        let token = std::str::from_utf8(&token)
            .map_err(|_| unavailable())?
            .trim();
        CloudflareProvisioner::new(
            &self.cloudflare_account_id,
            &self.cloudflare_zone_id,
            &self.cloudflare_zone_name,
            token,
        )
        .map_err(|_| unavailable())
    }
}

fn installation_hostname(id: Uuid, zone: &str) -> String {
    format!("u-{}.{zone}", id.simple())
}

struct Operator {
    origin: Url,
    mailer: SmtpMailer,
    email_key: [u8; 32],
    cloudflare: CloudflareProvisioner,
    zone: String,
    hub_port: u16,
    mcp_paths: Vec<String>,
    selectable_mcp_paths: Vec<String>,
    registry: Mutex<Registry>,
    home: PathBuf,
    account_id: String,
    zone_id: String,
}

fn unavailable() -> io::Error {
    io::Error::other("registration configuration or private state is unavailable")
}

fn read_private(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    private_file::open_private_file(path)
        .map_err(|_| unavailable())?
        .take(private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| unavailable())?;
    if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err(unavailable());
    }
    Ok(bytes)
}

fn https_origin(value: &str) -> io::Result<Url> {
    let url = Url::parse(value).map_err(|_| unavailable())?;
    if url.scheme() != "https"
        || url.domain().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(unavailable());
    }
    Ok(url)
}

fn epoch_seconds() -> Result<i64, HttpError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|time| i64::try_from(time.as_secs()).ok())
        .ok_or(HttpError::TemporarilyUnavailable)
}

fn registry_error(error: RegistryError) -> HttpError {
    match error {
        RegistryError::InvalidRequest => HttpError::InvalidRequest,
        RegistryError::NotFound => HttpError::RegistrationNotFound,
        RegistryError::Expired => HttpError::RegistrationExpired,
        RegistryError::Pending => HttpError::LoginPending,
        RegistryError::Revoked => HttpError::RegistrationRevoked,
        RegistryError::RateLimited => HttpError::RateLimited,
        RegistryError::Unavailable => HttpError::TemporarilyUnavailable,
    }
}

impl Operator {
    fn from_home(home: &Path) -> io::Result<Self> {
        let config = Config::read(home)?;
        let origin = https_origin(&config.registration_origin)?;
        let cloudflare = config.cloudflare(home)?;
        let password = read_private(&home.join("smtp.key"))?;
        let password = std::str::from_utf8(&password)
            .map_err(|_| unavailable())?
            .trim();
        let mailer = SmtpMailer::new(config.email_smtp.ok_or_else(unavailable)?, password)
            .ok_or_else(unavailable)?;
        let email_key = read_private(&home.join("email-otp.key"))?
            .try_into()
            .map_err(|_| unavailable())?;
        if config.invited_accounts.is_some() {
            eprintln!("Warning: invited_accounts is obsolete and does not restrict registration.");
        }
        let mut mcp_paths = config.mcp_path.into_iter().collect::<Vec<_>>();
        for path in config.mcp_paths {
            if !mcp_paths.contains(&path) {
                mcp_paths.push(path);
            }
        }
        let selectable_mcp_paths = config
            .selectable_mcp_paths
            .unwrap_or_else(|| mcp_paths.clone());
        if config.hub_port == 0
            || !mcp_paths
                .iter()
                .chain(&selectable_mcp_paths)
                .all(|path| valid_public_mcp_path(path))
        {
            return Err(unavailable());
        }
        Ok(Self {
            mailer,
            email_key,
            cloudflare,
            origin,
            zone: config.cloudflare_zone_name,
            hub_port: config.hub_port,
            mcp_paths,
            selectable_mcp_paths,
            registry: Mutex::new(
                Registry::open(&home.join("enrollments.sqlite3")).map_err(|_| unavailable())?,
            ),
            home: home.to_path_buf(),
            account_id: config.cloudflare_account_id,
            zone_id: config.cloudflare_zone_id,
        })
    }

    fn record<T>(
        &self,
        f: impl FnOnce(&mut Registry) -> Result<T, RegistryError>,
    ) -> Result<T, HttpError> {
        let mut store = self
            .registry
            .lock()
            .map_err(|_| HttpError::TemporarilyUnavailable)?;
        f(&mut store).map_err(registry_error)
    }

    fn hostname(&self, id: Uuid) -> String {
        installation_hostname(id, &self.zone)
    }

    fn public_url(&self, id: Uuid) -> String {
        format!("https://{}", self.hostname(id))
    }

    fn trusted_request<B>(&self, request: &Request<B>) -> bool {
        let host = request.headers().get_all(header::HOST);
        let mut values = host.iter();
        if values.next().and_then(|value| value.to_str().ok()) != self.origin.host_str()
            || values.next().is_some()
        {
            return false;
        }
        if request.method() == Method::POST && request.uri().path().starts_with("/register/") {
            let origin = request.headers().get_all(header::ORIGIN);
            let mut values = origin.iter();
            return values.next().and_then(|value| value.to_str().ok())
                == Some(self.origin.as_str().trim_end_matches('/'))
                && values.next().is_none();
        }
        true
    }

    #[cfg(test)]
    async fn finish(&self, id: Uuid, proof: String) -> Result<RegistrationResponse, HttpError> {
        self.finish_at(id, proof, None, None).await
    }

    async fn finish_at(
        &self,
        id: Uuid,
        proof: String,
        unix: Option<UnixOrigin>,
        routes: Option<Vec<String>>,
    ) -> Result<RegistrationResponse, HttpError> {
        if routes.is_none() && unix.is_some() && self.mcp_paths.is_empty() {
            return Err(HttpError::TemporarilyUnavailable);
        }
        let now = epoch_seconds()?;
        let record = self.record(|reg| reg.get(id, &proof, now))?;
        require_approved(&record)?;
        let _single_write = self
            .provider_guard()
            .map_err(|_| HttpError::TemporarilyUnavailable)?;
        let now = epoch_seconds()?;
        let record = self.record(|reg| reg.get(id, &proof, now))?;
        require_approved(&record)?;

        self.check_finish_lifecycle(id)
            .map_err(|_| HttpError::TemporarilyUnavailable)?;
        let mut resources = self
            .bound_resources(record.resources.as_deref())
            .map_err(|_| HttpError::TemporarilyUnavailable)?;
        let paths = match routes {
            Some(requested) => {
                if requested.len() > self.selectable_mcp_paths.len()
                    || requested
                        .iter()
                        .any(|path| !self.selectable_mcp_paths.contains(path))
                    || (resources.selected_routes.is_none() && record.resources.is_some())
                {
                    return Err(HttpError::InvalidRequest);
                }
                let mut selected = resources.selected_routes.take().unwrap_or_default();
                if selected
                    .iter()
                    .any(|path| !self.selectable_mcp_paths.contains(path))
                {
                    return Err(HttpError::TemporarilyUnavailable);
                }
                for path in requested {
                    if !selected.contains(&path) {
                        selected.push(path);
                    }
                }
                resources.selected_routes = Some(selected.clone());
                selected
            }
            None if resources.selected_routes.is_some() => return Err(HttpError::InvalidRequest),
            None => self.mcp_paths.clone(),
        };
        self.record(|reg| reg.bind_origin(id, &proof, now, unix.as_ref()))?;
        if resources.selected_routes.is_some() {
            self.persist_route_resources(
                id,
                &resources,
                record
                    .resources
                    .as_deref()
                    .filter(|_| record.state == EnrollmentState::Ready),
            )
            .map_err(|_| HttpError::TemporarilyUnavailable)?;
        }
        let provisioned = self
            .cloudflare
            .provision_at(
                id,
                &self.hostname(id),
                self.hub_port,
                unix.as_ref(),
                &paths,
                &mut resources,
                |resources| self.persist_resources(id, resources),
            )
            .await
            .map_err(|_| HttpError::TemporarilyUnavailable)?;
        let now = epoch_seconds()?;
        self.record(|reg| reg.mark_ready(id, now))?;
        let public_url = provisioned.origin.clone();
        let mut response = json!({
            "state": "ready", "public_url": public_url,
            "connector_token": provisioned.into_connector_token()
        });
        if let Some(unix) = unix {
            response["origin"] = json!({
                "kind": "unix_v1", "nonce": unix.nonce(),
                "hub_port": self.hub_port, "mcp_paths": paths,
            });
        }
        Ok(json_response(StatusCode::OK, response))
    }

    async fn dispatch<B: hyper::body::Body<Data = bytes::Bytes>>(
        &self,
        req: Request<B>,
    ) -> Result<RegistrationResponse, HttpError> {
        if !self.trusted_request(&req) {
            return Err(HttpError::RegistrationNotFound);
        }
        let request = parse_request(req).await?;
        let now = epoch_seconds()?;
        match request {
            RequestKind::Begin {
                request_id,
                user_code,
                proof,
            } => {
                let record = self.record(|reg| reg.begin(request_id, &proof, &user_code, now))?;
                let mut login_url = self
                    .origin
                    .join(&format!("register/{}", record.id))
                    .map_err(|_| HttpError::TemporarilyUnavailable)?;
                if let Some(handoff) =
                    self.record(|reg| reg.handoff_url_secret(record.id, &proof, now))?
                {
                    login_url.set_fragment(Some(&handoff));
                }
                let expires_at = chrono::DateTime::from_timestamp(record.expires_at, 0)
                    .ok_or(HttpError::TemporarilyUnavailable)?
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "installation_id": record.id.to_string(), "public_url": self.public_url(record.id),
                        "login_url": login_url.as_str(), "expires_at": expires_at
                    }),
                ))
            }
            RequestKind::Status {
                installation_id,
                proof,
            } => {
                let record = self.record(|reg| reg.get(installation_id, &proof, now))?;
                let state = match record.state {
                    EnrollmentState::Pending => "pending",
                    EnrollmentState::Approved => "approved",
                    EnrollmentState::Ready => "ready",
                    EnrollmentState::Revoked => "revoked",
                };
                Ok(json_response(
                    StatusCode::OK,
                    json!({
                        "state": state, "lifecycle": record.phase.name(),
                        "public_url": self.public_url(record.id)
                    }),
                ))
            }
            RequestKind::Finish {
                installation_id,
                proof,
            } => self.finish_at(installation_id, proof, None, None).await,
            RequestKind::FinishUnix {
                installation_id,
                proof,
                origin,
                routes,
            } => {
                self.finish_at(installation_id, proof, Some(origin), routes)
                    .await
            }
            RequestKind::RegisterPage { installation_id } => {
                let login = self.record(|reg| reg.login(installation_id, now))?;
                if login.expires_at <= now {
                    return Err(HttpError::RegistrationExpired);
                }
                login_form(login.id, &self.hostname(login.id))
            }
            RequestKind::RegisterSubmit {
                installation_id,
                handoff,
                email,
            } => {
                let attempt = self.record(|reg| {
                    reg.start_handoff_attempt(
                        installation_id,
                        &handoff,
                        &email,
                        &self.email_key,
                        now,
                    )
                })?;
                // Keep success and delivery failure indistinguishable in the request form.
                if self.mailer.send_code(&email, &attempt.code).await.is_some() {
                    let now = epoch_seconds()?;
                    self.record(|reg| reg.mark_email_sent(attempt.id, installation_id, now))?;
                }
                Ok(otp_form(installation_id, attempt.id))
            }
            RequestKind::VerifyEmail {
                installation_id,
                otp,
                attempt,
            } => {
                self.record(|reg| {
                    reg.verify_email_code(installation_id, attempt, &otp, &self.email_key, now)
                })?;
                Ok(approval_result())
            }
        }
    }
}

fn require_approved(record: &Enrollment) -> Result<(), HttpError> {
    match record.state {
        EnrollmentState::Approved => Ok(()),
        EnrollmentState::Ready if record.issued_at.is_some() => Ok(()),
        EnrollmentState::Ready => Err(HttpError::TemporarilyUnavailable),
        EnrollmentState::Pending => Err(HttpError::LoginPending),
        EnrollmentState::Revoked => Err(HttpError::RegistrationRevoked),
    }
}

fn lifecycle_view(record: &LifecycleRecord) -> serde_json::Value {
    json!({
        "installation_id": record.id.to_string(),
        "lifecycle": record.phase.name(),
        "provider_resources_recorded": record.resources.is_some(),
        "cloudflare_connection_previously_seen": record.ever_connected,
    })
}

async fn inspect_cloudflare(
    home: &Path,
    record: &LifecycleRecord,
) -> io::Result<serde_json::Value> {
    let config = Config::read(home)?;
    let resources = hosted_registration_lifecycle::bound_resources(
        &config.cloudflare_account_id,
        &config.cloudflare_zone_id,
        record.resources.as_deref(),
    )?;
    let hostname = installation_hostname(record.id, &config.cloudflare_zone_name);
    let target = SavedCloudflareInstallation {
        installation_id: record.id,
        account_id: &config.cloudflare_account_id,
        zone_id: &config.cloudflare_zone_id,
        hostname: &hostname,
        resources: &resources,
    };
    let cloudflare = config.cloudflare(home)?;
    let view = cloudflare.inspect_owned(&target).await;
    Ok(match view {
        Ok(view) => view,
        Err(error) => json!({"state": "unverified", "reason": error.to_string()}),
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let options = Options::parse();
    let home = options.home.canonicalize().map_err(|_| unavailable())?;
    if options.list || options.status.is_some() {
        let registry = Registry::open_for_inspection(&home.join("enrollments.sqlite3"))
            .map_err(|_| unavailable())?;
        if let Some(id) = options.status {
            let record = registry.lifecycle(id).map_err(|_| unavailable())?;
            let mut view = lifecycle_view(&record);
            if options.check_cloudflare {
                view["cloudflare"] = if record.resources.is_some() {
                    inspect_cloudflare(&home, &record).await?
                } else {
                    json!({"state": "no_recorded_resources"})
                };
            }
            println!("{view}");
        } else {
            let records = registry.list_lifecycle().map_err(|_| unavailable())?;
            println!(
                "{}",
                json!({"installations": records.iter().map(lifecycle_view).collect::<Vec<_>>()})
            );
        }
        return Ok(());
    }
    if let (Some(id), Some(app_id)) = (options.track_dot_access, options.access_app) {
        if !home.join("enrollments.sqlite3").is_file() {
            return Err(unavailable());
        }
        Operator::from_home(&home)?
            .track_dot_access(id, &app_id)
            .await?;
        println!("dot_access_tracked");
        return Ok(());
    }
    if let Some(id) = options.revoke.or(options.reissue) {
        if !home.join("enrollments.sqlite3").is_file() {
            return Err(unavailable());
        }
        let operator = Operator::from_home(&home)?;
        if options.revoke.is_some() {
            operator.revoke_installation(id).await?;
            println!("revoked");
        } else {
            operator.reissue_installation(id)?;
            println!("reissue_ready");
        }
        return Ok(());
    }
    let _process_lock = arrival_notify::lock_state(&home.join("enrollments.sqlite3"))
        .map_err(|_| io::Error::other("registration home is in use or unavailable"))?;
    let operator = Arc::new(Operator::from_home(&home)?);
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, options.port.ok_or_else(unavailable)?)).await?;
    tokio::spawn(Arc::clone(&operator).cleanup_loop());
    let connections = Arc::new(Semaphore::new(32));
    loop {
        let (stream, _) = listener.accept().await?;
        let Ok(permit) = connections.clone().try_acquire_owned() else {
            continue;
        };
        let operator = operator.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let server = service_fn(move |request| {
                let operator = operator.clone();
                async move {
                    let result = operator
                        .dispatch(request)
                        .await
                        .unwrap_or_else(HttpError::response);
                    Ok::<_, Infallible>(result)
                }
            });
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(5))
                .keep_alive(false)
                .serve_connection(TokioIo::new(stream), server)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use bytes::Bytes;
    use http_body_util::{BodyExt, Full};
    use serde_json::Value;

    pub(crate) fn fixture() -> (tempfile::TempDir, Operator) {
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(
            home.path(),
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .unwrap();
        let settings = json!({
            "registration_origin": "https://init.example.com",
            "email_smtp": {
                "host": "smtp.example.test",
                "port": 587,
                "username": "mailer@example.test",
                "from": "mailer@example.test"
            },
            "cloudflare_account_id": "a".repeat(32),
            "cloudflare_zone_id": "b".repeat(32),
            "cloudflare_zone_name": "example.com",
            "hub_port": 54843,
            "mcp_path": "/muse/mcp"
        });
        private_file::replace_private(
            &home.path().join("settings.json"),
            &serde_json::to_vec(&settings).unwrap(),
        )
        .unwrap();
        private_file::replace_private(&home.path().join("cloudflare.key"), b"fixture").unwrap();
        private_file::replace_private(&home.path().join("smtp.key"), b"fixture").unwrap();
        private_file::replace_private(&home.path().join("email-otp.key"), &[42; 32]).unwrap();
        let operator = Operator::from_home(home.path()).unwrap();
        (home, operator)
    }

    pub(crate) async fn call(
        operator: &Operator,
        method: Method,
        path: &str,
        proof: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "init.example.com")
            .header(header::AUTHORIZATION, format!("Bearer {proof}"));
        let body = match body {
            Some(body) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                body.to_string()
            }
            None => String::new(),
        };
        let response = operator
            .dispatch(request.body(Full::new(Bytes::from(body))).unwrap())
            .await
            .unwrap_or_else(HttpError::response);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let (head, body) = response.into_parts();
        let body = body.collect().await.unwrap().to_bytes();
        (head.status, serde_json::from_slice(&body).unwrap())
    }

    #[test]
    fn provider_writes_serialize_and_saved_resources_cannot_cross_zones() {
        let (home, operator) = fixture();
        let second = Operator::from_home(home.path()).unwrap();
        let guard = operator.provider_guard().unwrap();
        assert!(second.provider_guard().is_err());
        drop(guard);
        assert!(second.provider_guard().is_ok());

        let wrong_zone = serde_json::to_vec(&json!({
            "account_id": "a".repeat(32),
            "zone_id": "c".repeat(32),
            "resources": CloudflareResources::default(),
        }))
        .unwrap();
        assert!(operator.bound_resources(Some(&wrong_zone)).is_err());
    }

    async fn smtp_fixture(
        operator: &mut Operator,
        accepts_delivery: bool,
    ) -> tokio::task::JoinHandle<String> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        operator
            .mailer
            .use_loopback_fixture(listener.local_addr().unwrap().port());
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            stream
                .get_mut()
                .write_all(b"220 fixture ESMTP\r\n")
                .await
                .unwrap();
            let mut message = String::new();
            loop {
                let mut line = String::new();
                if stream.read_line(&mut line).await.unwrap() == 0 {
                    break;
                }
                let response: &[u8] = if line.starts_with("EHLO ") {
                    b"250 fixture\r\n"
                } else if line.starts_with("MAIL FROM:") || line.starts_with("RCPT TO:") {
                    b"250 ok\r\n"
                } else if line == "DATA\r\n" {
                    stream
                        .get_mut()
                        .write_all(b"354 send mail\r\n")
                        .await
                        .unwrap();
                    loop {
                        line.clear();
                        assert_ne!(stream.read_line(&mut line).await.unwrap(), 0);
                        if line == ".\r\n" {
                            break;
                        }
                        message.push_str(&line);
                    }
                    if accepts_delivery {
                        b"250 queued\r\n"
                    } else {
                        b"451 unavailable\r\n"
                    }
                } else if line == "QUIT\r\n" {
                    stream.get_mut().write_all(b"221 bye\r\n").await.unwrap();
                    break;
                } else {
                    panic!("unexpected SMTP command");
                };
                stream.get_mut().write_all(response).await.unwrap();
            }
            message
        })
    }

    #[tokio::test]
    async fn installer_retry_keeps_its_address_and_cannot_finish_before_approval() {
        let (_home, mut operator) = fixture();
        let upstream = smtp_fixture(&mut operator, false).await;
        let proof = URL_SAFE_NO_PAD.encode([1; 32]);
        let other_proof = URL_SAFE_NO_PAD.encode([2; 32]);
        let begin = json!({ "request_id": Uuid::new_v4().to_string(), "user_code": "A1B2C3D4" });
        let (status, first) = call(
            &operator,
            Method::POST,
            "/v1/enrollments",
            &proof,
            Some(begin.clone()),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (_, retry) = call(
            &operator,
            Method::POST,
            "/v1/enrollments",
            &proof,
            Some(begin),
        )
        .await;
        assert_eq!(first, retry);
        let path = format!(
            "/v1/enrollments/{}",
            first["installation_id"].as_str().unwrap()
        );
        let (status, pending) = call(&operator, Method::GET, &path, &proof, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(pending["state"], "pending");
        assert_eq!(pending["lifecycle"], "pending");
        assert_eq!(pending["public_url"], first["public_url"]);
        let (status, _) = call(&operator, Method::GET, &path, &other_proof, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = call(
            &operator,
            Method::POST,
            &format!("{path}/finish"),
            &proof,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, json!({"error": "login_pending"}));

        let install_id = first["installation_id"].as_str().unwrap();
        let login_path = format!("/register/{install_id}");
        let link = Url::parse(first["login_url"].as_str().unwrap()).unwrap();
        let handoff = link.fragment().unwrap();
        let form = |origin: &'static str| {
            Request::builder()
                .method(Method::POST)
                .uri(&login_path)
                .header(header::HOST, "init.example.com")
                .header(header::ORIGIN, origin)
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Full::new(Bytes::from(format!(
                    "handoff={handoff}&email=not-invited%40example.test"
                ))))
                .unwrap()
        };
        assert!(matches!(
            operator.dispatch(form("https://other.example.com")).await,
            Err(HttpError::RegistrationNotFound)
        ));
        let otp = operator
            .dispatch(form("https://init.example.com"))
            .await
            .unwrap();
        assert_eq!(otp.status(), StatusCode::OK);
        assert!(
            otp.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .starts_with("__Host-pigeonmq-registration=")
        );
        upstream.await.unwrap();
    }

    #[tokio::test]
    async fn email_otp_is_bound_to_one_installation_and_one_attempt() {
        let (home, mut operator) = fixture();
        let db = home.path().join("enrollments.sqlite3");
        let upstream = smtp_fixture(&mut operator, true).await;

        let now = epoch_seconds().unwrap();
        let proof_a = URL_SAFE_NO_PAD.encode([1; 32]);
        let proof_b = URL_SAFE_NO_PAD.encode([2; 32]);
        let a = operator
            .record(|reg| reg.begin(Uuid::new_v4(), &proof_a, "A1B2C3D4", now))
            .unwrap();
        let b = operator
            .record(|reg| reg.begin(Uuid::new_v4(), &proof_b, "A1B2C3D4", now))
            .unwrap();
        let browser = |path: String, cookie: Option<String>, body: &str| {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::HOST, "init.example.com")
                .header(header::ORIGIN, "https://init.example.com")
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
            if let Some(cookie) = cookie {
                req = req.header(header::COOKIE, cookie);
            }
            req.body(Full::new(Bytes::copy_from_slice(body.as_bytes())))
                .unwrap()
        };
        let request_otp = |id, proof: &str| {
            let handoff = operator
                .record(|reg| reg.handoff_url_secret(id, proof, now))
                .unwrap()
                .unwrap();
            browser(
                format!("/register/{id}"),
                None,
                &format!("handoff={handoff}&email=invited%40example.test"),
            )
        };
        let submit = |id, cookie, otp| browser(format!("/register/{id}/verify"), Some(cookie), otp);
        let read_cookie = |response: RegistrationResponse| {
            response.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .to_owned()
        };

        let cookie = read_cookie(
            operator
                .dispatch(request_otp(a.id, &proof_a))
                .await
                .unwrap(),
        );
        let mail = upstream.await.unwrap();
        assert!(mail.contains("invited@example.test"));
        let otp = mail
            .split_once("verification code is ")
            .unwrap()
            .1
            .split(|c: char| !c.is_ascii_digit())
            .next()
            .unwrap();
        assert!(hosted_email::valid_otp(otp));
        let form_code = format!("otp={otp}");
        let pending = std::fs::read(&db).unwrap();
        assert!(
            !pending
                .windows(b"invited@example.test".len())
                .any(|bytes| bytes == b"invited@example.test")
        );
        assert!(matches!(
            operator.dispatch(request_otp(b.id, &proof_b)).await,
            Err(HttpError::RateLimited)
        ));
        assert!(matches!(
            operator
                .dispatch(submit(b.id, cookie.clone(), &form_code))
                .await,
            Err(HttpError::RegistrationNotFound)
        ));
        assert!(
            operator
                .dispatch(submit(a.id, cookie.clone(), &form_code))
                .await
                .is_ok()
        );
        assert!(matches!(
            operator.dispatch(submit(a.id, cookie, &form_code)).await,
            Err(HttpError::RegistrationNotFound)
        ));
        assert_eq!(
            operator
                .record(|reg| reg.get(a.id, &proof_a, now))
                .unwrap()
                .state,
            EnrollmentState::Approved
        );
        assert_eq!(
            operator
                .record(|reg| reg.get(b.id, &proof_b, now))
                .unwrap()
                .state,
            EnrollmentState::Pending
        );

        assert!(matches!(
            operator.finish(b.id, proof_b).await,
            Err(HttpError::LoginPending)
        ));
        let connection = rusqlite::Connection::open(db).unwrap();
        let attempts = connection
            .query_row("SELECT COUNT(*) FROM browser_attempts", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(attempts, 0);
    }
}
