//! Resume a single hosted enrollment without exposing browser or machine secrets.
use crate::{
    cloud_setup,
    hosted_origin::UnixOrigin,
    hosted_origin_local,
    hosted_registration::{self, OriginDescriptor},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use pigeonmq::{arrival_notify::lock_state, private_file};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, DirBuilder},
    io::{self, Read},
    os::unix::fs::DirBuilderExt,
    path::Path,
    time::{Duration, Instant},
};
use url::Url;
use uuid::Uuid;

const FILE: &str = "hosted-enrollment.json";
const RESUME: &str = "Run the same init --reserve-address command with the same --home to resume.";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    origin: String,
    request_id: String,
    user_code: String,
    proof: String,
    binding: Option<Binding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_descriptor: Option<OriginDescriptor>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    installation_id: String,
    public_url: String,
    expires_at: String,
}

fn https_origin(input: &str) -> Result<Url, String> {
    if !input.starts_with("https://")
        || input
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err("registration origin must be an HTTPS origin".into());
    }
    Url::parse(input)
        .ok()
        .filter(|url| {
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(|| "registration origin must be an HTTPS origin".into())
}

fn valid_secret(value: &str) -> bool {
    value.len() == 43
        && URL_SAFE_NO_PAD
            .decode(value)
            .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(bytes) == value)
}

fn new_enrollment(origin: &Url) -> Enrollment {
    let mut bytes = [0u8; 32];
    bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    Enrollment {
        origin: origin.to_string(),
        request_id: Uuid::new_v4().to_string(),
        user_code: Uuid::new_v4().simple().to_string()[..8].to_uppercase(),
        proof: URL_SAFE_NO_PAD.encode(bytes),
        binding: None,
        unix_nonce: None,
        unix_descriptor: None,
    }
}

fn load(path: &Path) -> Result<Option<Enrollment>, String> {
    match path.symlink_metadata() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err("cannot inspect hosted enrollment state".into()),
        Ok(_) => {}
    }
    let file = private_file::open_private_file(path)
        .map_err(|_| "hosted enrollment state must be an owner-private file")?;
    let limit = private_file::MAX_PRIVATE_CONFIG_BYTES;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "cannot read hosted enrollment state")?;
    if bytes.len() as u64 > limit {
        return Err("hosted enrollment state is too large".into());
    }
    let saved: Enrollment =
        serde_json::from_slice(&bytes).map_err(|_| "invalid hosted enrollment state")?;
    if !Uuid::parse_str(&saved.request_id).is_ok_and(|id| {
        id.get_version() == Some(uuid::Version::Random) && id.to_string() == saved.request_id
    }) || !valid_secret(&saved.proof)
        || saved.user_code.len() != 8
        || !saved
            .user_code
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte.is_ascii_uppercase())
    {
        return Err("invalid hosted enrollment state".into());
    }
    let unix = saved
        .unix_nonce
        .as_deref()
        .map(|value| UnixOrigin::parse(value).ok_or("invalid hosted Unix origin intent"))
        .transpose()?;
    if let Some(descriptor) = &saved.unix_descriptor {
        descriptor.validate(unix.as_ref().ok_or("missing hosted Unix origin intent")?)?;
    }
    Ok(Some(saved))
}

fn save(path: &Path, enrollment: &Enrollment) -> Result<(), String> {
    let bytes = serde_json::to_vec(enrollment).map_err(|_| "invalid hosted enrollment state")?;
    private_file::replace_private(path, &bytes)
        .map_err(|_| "cannot save hosted enrollment state".into())
}

fn login_link(origin: &Url, id: Uuid, input: &str, proof: &str) -> Result<Option<Url>, String> {
    let url = Url::parse(input).map_err(|_| "invalid hosted browser link")?;
    if input != url.as_str()
        || url.origin() != origin.origin()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != format!("/register/{id}")
        || url.query().is_some()
    {
        return Err("invalid hosted browser link".into());
    }
    match url.fragment() {
        None => Ok(None), // The operator has already consumed the handoff.
        Some(secret) if valid_secret(secret) && secret != proof => Ok(Some(url)),
        _ => Err("invalid hosted browser link".into()),
    }
}

fn open_browser(link: &Url) -> Result<(), String> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    let failure = || "cannot open the default browser".to_owned();
    // Pass the secret in stdin, not in the process arguments or its environment.
    // The script quotes the URL as data and asks AppKit to choose the browser.
    let quoted = serde_json::to_string(link.as_str()).map_err(|_| failure())?;
    let mut child = Command::new("/usr/bin/osascript")
        .args(["-l", "JavaScript", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| failure())?;
    let written = writeln!(
        child.stdin.take().ok_or_else(failure)?,
        r#"ObjC.import("AppKit");
if (!$.NSWorkspace.sharedWorkspace.openURL($.NSURL.URLWithString({quoted}))) throw Error("browser");"#
    );
    let status = child.wait();
    if written.is_ok() && status.is_ok_and(|status| status.success()) {
        Ok(())
    } else {
        Err(failure())
    }
}

#[derive(PartialEq, Eq)]
enum OriginIntent {
    Legacy,
    UnixV1,
}

#[allow(
    clippy::too_many_arguments,
    reason = "Keep the enrollment options and injected browser and activation boundaries explicit"
)]
fn enroll(
    home: &Path,
    origin: &Url,
    api: &str,
    wait: Duration,
    poll: Duration,
    intent: OriginIntent,
    open: impl FnOnce(&Url) -> Result<(), String>,
    finalize: impl FnOnce(&str, &str, &str) -> Result<(), String>,
) -> Result<String, String> {
    let path = home.join(FILE);
    let _lock = lock_state(&path).map_err(|_| "hosted enrollment is busy or its lock is unsafe")?;
    let mut saved = match load(&path)? {
        Some(saved) if saved.origin != origin.as_str() => {
            return Err("use the original HTTPS registration origin for this home".into());
        }
        Some(saved) if saved.unix_nonce.is_some() == (intent == OriginIntent::UnixV1) => saved,
        Some(_) => return Err("use the original hosted origin mode for this home".into()),
        None => {
            let mut enrollment = new_enrollment(origin);
            if intent == OriginIntent::UnixV1 {
                let nonce = hosted_origin_local::new_origin()
                    .map_err(|_| "cannot select a hosted Unix origin")?;
                enrollment.unix_nonce = Some(nonce.nonce().to_owned());
            }
            save(&path, &enrollment)?;
            enrollment
        }
    };
    let unix = saved.unix_nonce.as_deref().and_then(UnixOrigin::parse);

    // Retry the same begin even if the last call reached the server but lost its response.
    let procedure = (|| -> Result<String, String> {
        let reg =
            hosted_registration::begin(api, &saved.request_id, &saved.user_code, &saved.proof)?;
        let installation_id =
            Uuid::parse_str(&reg.installation_id).map_err(|_| "invalid hosted installation ID")?;
        if installation_id.to_string() != reg.installation_id {
            return Err("invalid hosted installation ID".into());
        }
        https_origin(&reg.public_url)?;
        chrono::DateTime::parse_from_rfc3339(&reg.expires_at)
            .map_err(|_| "invalid hosted enrollment expiry")?;
        let binding = Binding {
            installation_id: reg.installation_id.clone(),
            public_url: reg.public_url,
            expires_at: reg.expires_at,
        };
        let link = login_link(origin, installation_id, &reg.login_url, &saved.proof)?;
        if saved.binding.as_ref().is_some_and(|old| old != &binding) {
            return Err("hosted enrollment binding changed".into());
        }
        if saved.binding.is_none() {
            saved.binding = Some(binding);
            save(&path, &saved)?;
        }
        let binding = saved
            .binding
            .as_ref()
            .ok_or("missing hosted enrollment binding")?;

        let deadline = Instant::now() + wait;
        let mut opener = Some(open);
        loop {
            let status = hosted_registration::status(api, &reg.installation_id, &saved.proof)?;
            if status.public_url != binding.public_url {
                return Err("hosted enrollment binding changed".into());
            }
            match status.state.as_str() {
                "approved" | "ready" => break,
                "pending" => {
                    if let Some(open) = opener.take()
                        && let Some(link) = link.as_ref()
                    {
                        // The OS opener receives the link; the CLI never prints or saves it.
                        open(link)?;
                        eprintln!(
                            "Approve this installation in your browser. Waiting for approval."
                        );
                    }
                }
                "revoked" => return Err("registration_revoked".into()),
                _ => return Err("unexpected hosted enrollment state".into()),
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("browser approval is pending".into());
            }
            std::thread::sleep(remaining.min(poll));
        }
        let (public_url, token, descriptor) = if let Some(origin) = unix.as_ref() {
            let (url, token, scope) =
                hosted_registration::finish_unix(api, &reg.installation_id, &saved.proof, origin)?;
            (url, token, Some(scope))
        } else {
            let (url, token) =
                hosted_registration::finish(api, &reg.installation_id, &saved.proof)?;
            (url, token, None)
        };
        if public_url != binding.public_url
            || token.is_empty()
            || token.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES
            || token
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace())
        {
            return Err("invalid hosted finish response".into());
        }
        if let Some(descriptor) = descriptor {
            if saved
                .unix_descriptor
                .as_ref()
                .is_some_and(|previous| previous != &descriptor)
            {
                return Err("hosted Unix origin binding changed".into());
            }
            if saved.unix_descriptor.is_none() {
                saved.unix_descriptor = Some(descriptor);
                save(&path, &saved)?;
            }
        }
        finalize(&public_url, &reg.installation_id, &token)?;
        Ok(public_url)
    })();

    match procedure {
        Err(reason) if reason == "registration_expired" => {
            fs::remove_file(&path).map_err(|_| "cannot clear expired enrollment state")?;
            fs::File::open(home)
                .and_then(|dir| dir.sync_all())
                .map_err(|_| "cannot sync cleared enrollment state")?;
            Err("Registration expired. Run the same command to request a new address.".into())
        }
        Err(reason) if reason == "registration_revoked" => Err(
            "Registration was revoked. Do not reuse this enrollment. Contact the operator.".into(),
        ),
        Err(reason) => Err(format!("{reason}. {RESUME}")),
        Ok(url) => Ok(url),
    }
}

pub(crate) fn staged_unix(home: &Path) -> Result<Option<(Uuid, OriginDescriptor)>, String> {
    let Some(saved) = load(&home.join(FILE))? else {
        return Ok(None);
    };
    let Some(descriptor) = saved.unix_descriptor else {
        return Ok(None);
    };
    let binding = saved.binding.ok_or("missing hosted installation binding")?;
    let id = Uuid::parse_str(&binding.installation_id)
        .map_err(|_| "invalid hosted installation binding")?;
    if id.is_nil() || id.to_string() != binding.installation_id {
        return Err("invalid hosted installation binding".into());
    }
    let staged = crate::hosted_connector::load_connection(home)?;
    if staged.as_ref() != Some(&(binding.public_url, binding.installation_id)) {
        return Err("hosted credentials do not belong to this enrollment".into());
    }
    private_file::open_private_file(&home.join(crate::hosted_connector::TOKEN_FILE))
        .map_err(|_| "hosted connector credential is unavailable")?;
    Ok(Some((id, descriptor)))
}

pub(crate) fn run(home: &Path, unix: bool) -> Result<(), String> {
    let configured =
        std::env::var("PIGEONMQ_REGISTRATION_URL").map_err(|_| "registration_url_required")?;
    let origin = https_origin(&configured)?;
    if !cfg!(target_os = "macos") {
        return Err("hosted enrollment requires macOS".into());
    }
    match DirBuilder::new().mode(0o700).create(home) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("cannot create hosted hub home".into()),
    }
    let home =
        cloud_setup::require_home(home).map_err(|_| "hosted hub home must be owner-private")?;
    let public_url = enroll(
        &home,
        &origin,
        origin.as_str(),
        Duration::from_secs(90),
        Duration::from_secs(2),
        if unix {
            OriginIntent::UnixV1
        } else {
            OriginIntent::Legacy
        },
        open_browser,
        |url, id, token| crate::hosted_connector::stage(&home, url, id, token),
    )?;
    println!("reserved_address={public_url}");
    let scheduled = if unix {
        match crate::lifecycle::schedule_hosted_unix(&home) {
            Ok(value) => value,
            Err(err) => {
                println!("hosted_connector=staged");
                println!("hosted_public_route=unverified");
                return Err(format!("The hosted credential is still staged: {err}"));
            }
        }
    } else {
        false
    };
    println!(
        "hosted_connector={}",
        if scheduled { "scheduled" } else { "staged" }
    );
    println!("hosted_public_route=unverified");
    if !scheduled {
        if unix {
            eprintln!("Activate the required managed profiles, then run the same reserve command.");
        } else {
            eprintln!(
                "The connector credential is saved. This preview does not start a hosted connector."
            );
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn stage_unix_fixture(home: &Path, id: Uuid, port: u16, paths: &[&str]) {
    let origin = https_origin("https://init.example.test").unwrap();
    let mut saved = new_enrollment(&origin);
    let nonce = hosted_origin_local::new_origin().unwrap();
    let public_url = "https://u-fixture.example.test";
    saved.binding = Some(Binding {
        installation_id: id.to_string(),
        public_url: public_url.into(),
        expires_at: "2026-01-01T00:00:00Z".into(),
    });
    saved.unix_nonce = Some(nonce.nonce().to_owned());
    saved.unix_descriptor = Some(
        serde_json::from_value(serde_json::json!({
            "kind":"unix_v1", "nonce":nonce.nonce(), "hub_port":port, "mcp_paths":paths
        }))
        .unwrap(),
    );
    save(&home.join(FILE), &saved).unwrap();
    crate::hosted_connector::stage(home, public_url, &id.to_string(), "synthetic-token").unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        io::{BufRead, BufReader, Write},
        net::{TcpListener, TcpStream},
        os::unix::fs::PermissionsExt,
        thread,
    };

    fn temp_home() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        home
    }

    const ORIGIN: &str = "https://init.example.test";
    const PUBLIC: &str = "https://u-install.example.test";
    const ID: &str = "11111111-1111-4111-8111-111111111111";
    const OTHER: &str = "22222222-2222-4222-8222-222222222222";

    fn begin(base: &str, id: &str, fragment: Option<&str>) -> Value {
        let fragment = fragment
            .map(|value| format!("#{value}"))
            .unwrap_or_default();
        json!({
            "installation_id": id,
            "public_url": PUBLIC,
            "expires_at": "2020-01-01T00:15:00Z",
            "login_url": format!("{base}/register/{id}{fragment}"),
        })
    }

    fn read_request(stream: &TcpStream) -> (String, String, Vec<u8>) {
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request = line.trim().to_owned();
        let mut content_length = 0;
        let mut proof = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let lower = line.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap();
            }
            if let Some((header, value)) = line.split_once(':')
                && header.eq_ignore_ascii_case("authorization")
            {
                proof = value.trim().to_owned();
            }
            if line == "\r\n" {
                break;
            }
            assert!(!line.is_empty());
        }
        let mut body = vec![0; content_length];
        reader.read_exact(&mut body).unwrap();
        (request, proof, body)
    }

    fn operator(
        home: &Path,
        responses: Vec<(&'static str, Option<(u16, Value)>)>,
    ) -> (String, thread::JoinHandle<()>) {
        operator_with_intent(home, responses, false)
    }

    fn operator_with_intent(
        home: &Path,
        responses: Vec<(&'static str, Option<(u16, Value)>)>,
        expected_unix: bool,
    ) -> (String, thread::JoinHandle<()>) {
        let path = home.join(FILE);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let mut responses = VecDeque::from(responses);
            let deadline = Instant::now() + Duration::from_secs(12);
            while let Some((expected, response)) = responses.pop_front() {
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(err)
                            if err.kind() == io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            thread::sleep(Duration::from_millis(2))
                        }
                        Err(_) => panic!("operator did not receive an expected request"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                let (request, authorization, body) = read_request(&stream);
                assert_eq!(request, expected);
                // The first Begin is not allowed to reach the operator before the journal.
                let saved = load(&path).unwrap().unwrap();
                assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
                assert!(authorization == format!("Bearer {}", saved.proof));
                if request == "POST /v1/enrollments HTTP/1.1" {
                    assert_eq!(saved.unix_nonce.is_some(), expected_unix);
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert!(
                        body == json!({"request_id":saved.request_id, "user_code":saved.user_code})
                    );
                } else {
                    assert!(saved.binding.is_some());
                }
                if request.ends_with("/finish-unix HTTP/1.1") {
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(body, json!({"kind":"unix_v1", "nonce":saved.unix_nonce}));
                }
                if let Some((status, value)) = response {
                    let body = value.to_string();
                    write!(stream, "HTTP/1.1 {status} fixture\r\nContent-Length: {}\r\nConnection: close\r\nContent-Type: application/json\r\n\r\n{body}", body.len()).unwrap();
                    stream.flush().unwrap();
                }
            }
        });
        (format!("http://{address}"), server)
    }

    const BEGIN: &str = "POST /v1/enrollments HTTP/1.1";
    const STATUS: &str = "GET /v1/enrollments/11111111-1111-4111-8111-111111111111 HTTP/1.1";
    const FINISH: &str =
        "POST /v1/enrollments/11111111-1111-4111-8111-111111111111/finish HTTP/1.1";

    fn fixture(
        home: &Path,
        api: &str,
        open: impl FnOnce(&Url) -> Result<(), String>,
        finalize: impl FnOnce(&str, &str, &str) -> Result<(), String>,
    ) -> Result<String, String> {
        enroll(
            home,
            &https_origin(ORIGIN).unwrap(),
            api,
            Duration::ZERO,
            Duration::ZERO,
            OriginIntent::Legacy,
            open,
            finalize,
        )
    }

    #[test]
    fn unix_intent_survives_dropped_begin_and_refuses_downgrade_or_changed_reply() {
        let home = temp_home();
        let path = home.path().join(FILE);
        let origin = https_origin(ORIGIN).unwrap();
        let (drop_api, drop_server) = operator_with_intent(home.path(), vec![(BEGIN, None)], true);
        let run = |api: &str, intent| {
            enroll(
                home.path(),
                &origin,
                api,
                Duration::ZERO,
                Duration::ZERO,
                intent,
                |_| panic!("unexpected opener"),
                |_, _, _| Ok(()),
            )
        };
        assert!(
            run(&drop_api, OriginIntent::UnixV1)
                .unwrap_err()
                .contains(RESUME)
        );
        drop_server.join().unwrap();
        let first = load(&path).unwrap().unwrap();
        let nonce = first.unix_nonce.as_deref().unwrap();
        assert!(UnixOrigin::parse(nonce).is_some());
        assert!(!fs::read_to_string(&path).unwrap().contains("hub.sock"));
        assert!(
            run(&drop_api, OriginIntent::Legacy)
                .unwrap_err()
                .contains("origin mode")
        );
        let finish =
            "POST /v1/enrollments/11111111-1111-4111-8111-111111111111/finish-unix HTTP/1.1";
        let provider = |nonce: &str, route: &str| {
            json!({
                "state":"ready", "public_url":PUBLIC, "connector_token":"fixture-token",
                "origin":{"kind":"unix_v1","nonce":nonce,"hub_port":54843,"mcp_paths":[route]}
            })
        };
        let (api, server) = operator_with_intent(
            home.path(),
            vec![
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (
                    STATUS,
                    Some((200, json!({"state":"approved","public_url":PUBLIC}))),
                ),
                (
                    finish,
                    Some((
                        200,
                        provider("00000000000000000000000000000000", "/muse/mcp"),
                    )),
                ),
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (
                    STATUS,
                    Some((200, json!({"state":"approved","public_url":PUBLIC}))),
                ),
                (finish, Some((200, provider(nonce, "/muse/mcp")))),
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (
                    STATUS,
                    Some((200, json!({"state":"ready","public_url":PUBLIC}))),
                ),
                (finish, Some((200, provider(nonce, "/dot/mcp")))),
            ],
            true,
        );
        assert!(
            run(&api, OriginIntent::UnixV1)
                .unwrap_err()
                .contains("binding changed")
        );
        assert!(load(&path).unwrap().unwrap().unix_descriptor.is_none());
        assert_eq!(run(&api, OriginIntent::UnixV1).unwrap(), PUBLIC);
        assert!(
            run(&api, OriginIntent::UnixV1)
                .unwrap_err()
                .contains("binding changed")
        );
        let stored = load(&path).unwrap().unwrap();
        assert_eq!(stored.request_id, first.request_id);
        assert!(stored.proof == first.proof && stored.unix_nonce == first.unix_nonce);
        assert_eq!(stored.unix_descriptor.unwrap().mcp_paths, ["/muse/mcp"]);
        server.join().unwrap();
    }

    #[test]
    fn dropped_begin_and_consumed_handoff_resume_same_binding() {
        let home = temp_home();
        let path = home.path().join(FILE);
        let secret = URL_SAFE_NO_PAD.encode([19; 32]);
        let (api, server) = operator(
            home.path(),
            vec![
                (BEGIN, None),
                (BEGIN, Some((200, begin(ORIGIN, ID, Some(&secret))))),
                (
                    STATUS,
                    Some((200, json!({"state":"pending","public_url":PUBLIC}))),
                ),
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (
                    STATUS,
                    Some((200, json!({"state":"approved","public_url":PUBLIC}))),
                ),
                (
                    FINISH,
                    Some((
                        200,
                        json!({"state":"ready","public_url":PUBLIC,"connector_token":"fixture-token"}),
                    )),
                ),
            ],
        );
        let no_open = |_: &Url| panic!("unexpected opener");
        let no_finish = |_: &str, _: &str, _: &str| panic!("unexpected finalize");
        let error = fixture(home.path(), &api, no_open, no_finish).unwrap_err();
        assert!(error.contains(RESUME), "{error}");
        let original = fs::read(&path).unwrap();
        let pending = fixture(
            home.path(),
            &api,
            |url| {
                assert_eq!(url.fragment(), Some(secret.as_str()));
                Ok(())
            },
            no_finish,
        )
        .unwrap_err();
        assert!(pending.starts_with("browser approval is pending"));
        assert!(!pending.contains(&secret));
        let captured = load(&path).unwrap().unwrap();
        let first: Enrollment = serde_json::from_slice(&original).unwrap();
        assert!(
            first.request_id == captured.request_id
                && first.proof == captured.proof
                && first.user_code == captured.user_code
        );
        assert!(!fs::read_to_string(&path).unwrap().contains(&secret));
        assert_eq!(
            fixture(
                home.path(),
                &api,
                |_| panic!("consumed link opened"),
                |url, id, token| {
                    assert_eq!((url, id, token), (PUBLIC, ID, "fixture-token"));
                    Ok(())
                }
            )
            .unwrap(),
            PUBLIC
        );
        server.join().unwrap();
    }

    #[test]
    fn changed_binding_and_untrusted_link_never_open_or_finish() {
        let home = temp_home();
        let (api, server) = operator(
            home.path(),
            vec![
                (
                    BEGIN,
                    Some((200, begin("https://other.example.test", ID, None))),
                ),
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (
                    STATUS,
                    Some((200, json!({"state":"pending","public_url":PUBLIC}))),
                ),
                (BEGIN, Some((200, begin(ORIGIN, OTHER, None)))),
            ],
        );
        let attempt = || {
            fixture(
                home.path(),
                &api,
                |_| panic!("unexpected opener"),
                |_, _, _| panic!("unexpected finalize"),
            )
            .unwrap_err()
        };
        assert!(attempt().starts_with("invalid hosted browser link"));
        assert!(attempt().starts_with("browser approval is pending"));
        assert!(attempt().starts_with("hosted enrollment binding changed"));
        server.join().unwrap();
    }

    #[test]
    fn only_authoritative_expiry_allows_new_begin() {
        let home = temp_home();
        let (api, server) = operator(
            home.path(),
            vec![
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (
                    STATUS,
                    Some((200, json!({"state":"pending","public_url":PUBLIC}))),
                ),
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (STATUS, Some((410, json!({"error":"registration_expired"})))),
            ],
        );
        let attempt = || {
            fixture(
                home.path(),
                &api,
                |_| panic!("unexpected opener"),
                |_, _, _| panic!("unexpected finalize"),
            )
            .unwrap_err()
        };
        assert!(attempt().starts_with("browser approval is pending"));
        assert!(home.path().join(FILE).exists());
        assert!(attempt().starts_with("Registration expired"));
        assert!(!home.path().join(FILE).exists());
        server.join().unwrap();
    }
}
