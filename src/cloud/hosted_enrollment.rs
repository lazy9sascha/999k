//! Resume a single hosted enrollment without exposing browser or machine secrets.
use crate::{
    cloud_setup,
    hosted_origin::UnixOrigin,
    hosted_origin_local,
    hosted_registration::{self, OriginDescriptor},
    registration_origin::https_origin,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use radio999k::{arrival_notify::lock_state, private_file};
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selected_mcp_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_routes: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Binding {
    installation_id: String,
    public_url: String,
    expires_at: String,
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
        selected_mcp_path: None,
        requested_routes: None,
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
    SelectableUnix,
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
    selected_mcp_path: Option<&str>,
    open: impl FnOnce(&Url) -> Result<(), String>,
    finalize: impl FnOnce(&str, &str, &str) -> Result<(), String>,
) -> Result<String, String> {
    let path = home.join(FILE);
    let _lock = lock_state(&path).map_err(|_| "hosted enrollment is busy or its lock is unsafe")?;
    let mut saved = match load(&path)? {
        Some(saved) if saved.origin != origin.as_str() => {
            return Err("use the original HTTPS registration origin for this home".into());
        }
        Some(saved)
            if saved.unix_nonce.is_some() == (intent != OriginIntent::Legacy)
                && saved.requested_routes.is_some() == (intent == OriginIntent::SelectableUnix) =>
        {
            saved
        }
        Some(_) => return Err("use the original hosted origin mode for this home".into()),
        None => {
            let mut enrollment = new_enrollment(origin);
            if intent != OriginIntent::Legacy {
                let nonce = hosted_origin_local::new_origin()
                    .map_err(|_| "cannot select a hosted Unix origin")?;
                enrollment.unix_nonce = Some(nonce.nonce().to_owned());
            }
            if intent == OriginIntent::SelectableUnix {
                enrollment.requested_routes = Some(Vec::new());
            } else {
                enrollment.selected_mcp_path = selected_mcp_path.map(str::to_owned);
            }
            save(&path, &enrollment)?;
            enrollment
        }
    };
    if intent != OriginIntent::SelectableUnix
        && let Some(selected) = selected_mcp_path
        && Some(selected) != saved.selected_mcp_path.as_deref()
        && !saved
            .unix_descriptor
            .as_ref()
            .is_some_and(|scope| exact_peer_scope(&scope.mcp_paths, selected))
    {
        return Err("This home has a different hosted enrollment. Preserve it and use an approved exact ingress for this peer.".into());
    }
    let previous_routes = saved.requested_routes.clone();
    if let Some(routes) = &mut saved.requested_routes
        && let Some(selected) = selected_mcp_path
        && !routes.iter().any(|path| path == selected)
    {
        routes.push(selected.to_owned());
        save(&path, &saved)?;
    }
    let required_path = saved
        .selected_mcp_path
        .clone()
        .or_else(|| selected_mcp_path.map(str::to_owned));
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
            if let Some(phase @ ("expiring" | "revoking" | "expired")) = status.lifecycle.as_deref()
            {
                return Err(lifecycle_notice(phase).into());
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
            let result = hosted_registration::finish_unix(
                api,
                &reg.installation_id,
                &saved.proof,
                origin,
                saved.requested_routes.as_deref(),
            );
            if matches!(&result, Err(reason) if reason == "http_400")
                && saved.requested_routes != previous_routes
            {
                saved.requested_routes = previous_routes;
                save(&path, &saved)?;
            }
            let (url, token, scope) = result.map_err(|reason| match reason.as_str() {
                "http_400" if selected_mcp_path.is_some() && saved.requested_routes.is_some() => {
                    "The registration provider did not accept the selected peer route".into()
                }
                _ => reason,
            })?;
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
        if let Some(descriptor) = descriptor.as_ref() {
            if let Some(requested) = &saved.requested_routes {
                if descriptor.mcp_paths.len() != requested.len()
                    || !requested
                        .iter()
                        .all(|path| descriptor.mcp_paths.contains(path))
                    || saved
                        .unix_descriptor
                        .as_ref()
                        .is_some_and(|previous| !descriptor.same_origin(previous))
                {
                    return Err("hosted Unix origin binding changed or provider did not approve the requested peer routes".into());
                }
            } else if required_path
                .as_deref()
                .is_some_and(|path| !exact_peer_scope(&descriptor.mcp_paths, path))
            {
                return Err("Registration provider did not approve exactly the selected peer route; connector remains unattached".into());
            }
            if saved.requested_routes.is_none()
                && saved
                    .unix_descriptor
                    .as_ref()
                    .is_some_and(|previous| previous != descriptor)
            {
                return Err("hosted Unix origin binding changed".into());
            }
        }
        finalize(&public_url, &reg.installation_id, &token)?;
        if descriptor.is_some() && saved.unix_descriptor != descriptor {
            saved.unix_descriptor = descriptor;
            save(&path, &saved)?;
        }
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
        Err(reason) if reason == "active_hosted_address" => Err(
            "This email already has a hosted address. Use the existing installation or your own HTTPS domain.".into(),
        ),
        Err(reason) if reason == "hosted_capacity" => Err(
            "Hosted addresses are full. Try again later or use your own HTTPS domain.".into(),
        ),
        Err(reason) if required_path.is_some() => Err(format!(
            "{reason}. Repeat the same selected-peer setup command to resume."
        )),
        Err(reason) => Err(format!("{reason}. {RESUME}")),
        Ok(url) => Ok(url),
    }
}

pub(crate) fn address_only_home(home: &Path) -> Result<bool, Box<dyn std::error::Error>> {
    if !has_selectable_reservation(home)?
        || !staged_unix(home)?.is_some_and(|(_, scope, _)| scope.mcp_paths.is_empty())
    {
        return Ok(false);
    }
    for entry in fs::read_dir(home)? {
        if !matches!(
            entry?.file_name().to_str(),
            Some(
                FILE | "hosted-enrollment.lock"
                    | "hosted-connection.json"
                    | crate::hosted_connector::TOKEN_FILE
            )
        ) {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn staged_unix(home: &Path) -> Result<Option<(Uuid, OriginDescriptor, String)>, String> {
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
    if staged.as_ref().is_none_or(|(url, installation)| {
        *url != binding.public_url || *installation != binding.installation_id
    }) {
        return Err("hosted credentials do not belong to this enrollment".into());
    }
    private_file::open_private_file(&home.join(crate::hosted_connector::TOKEN_FILE))
        .map_err(|_| "hosted connector credential is unavailable")?;
    Ok(Some((id, descriptor, binding.public_url)))
}

fn exact_peer_scope(paths: &[String], selected: &str) -> bool {
    paths == [selected]
}

const RELEASE_ORIGIN: &str = env!("RADIO999K_COMPILED_REGISTRATION_ORIGIN");

pub(crate) fn run(home: &Path, unix: bool) -> Result<(), String> {
    let saved = load(&home.join(FILE))?;
    let intent = match saved.as_ref() {
        Some(saved) if saved.requested_routes.is_some() => OriginIntent::SelectableUnix,
        Some(saved) if saved.unix_nonce.is_some() => OriginIntent::UnixV1,
        Some(_) if !unix => OriginIntent::Legacy,
        _ if unix => OriginIntent::UnixV1,
        _ => OriginIntent::SelectableUnix,
    };
    let origin = match saved {
        Some(saved) => https_origin(&saved.origin)?,
        None => registration_origin()?,
    };
    reserve(home, &origin, intent, None).map(|_| ())
}

pub(crate) fn selected_provider(peer: &str) -> Result<Url, String> {
    if crate::pairing::route(peer).is_none() {
        return Err("The selected peer does not support hosted HTTPS enrollment.".into());
    }
    registration_origin()
}

pub(crate) fn reserve_selected_peer(
    home: &Path,
    origin: &Url,
    route: &str,
) -> Result<String, String> {
    let intent = match load(&home.join(FILE))? {
        Some(saved) if saved.requested_routes.is_none() => OriginIntent::UnixV1,
        _ => OriginIntent::SelectableUnix,
    };
    reserve(home, origin, intent, Some(route))
}

pub(crate) fn has_selectable_reservation(home: &Path) -> Result<bool, String> {
    Ok(load(&home.join(FILE))?.is_some_and(|saved| saved.requested_routes.is_some()))
}

fn lifecycle_notice(phase: &str) -> &'static str {
    match phase {
        "pending" => "Approve the registration in your browser.",
        "approval_expired" => {
            "The approval expired. Run the same setup command to request a new address."
        }
        "approved" => "Run hosted setup again to finish reserving the address.",
        "ready" => "The registration is ready. Check each peer's connection in its app.",
        "expiring" => "The unused hosted connection is being removed.",
        "revoking" => "The operator has started removing public access.",
        "expired" => {
            "The unused hosted connection expired. Contact your operator to restore the address."
        }
        "revoked" => "Public access was revoked. Contact your operator to restore the address.",
        _ => "Hosted registration status is unavailable.",
    }
}

pub(crate) fn print_status(home: &Path) -> Result<(), String> {
    let home =
        cloud_setup::require_home(home).map_err(|_| "hosted hub home must be owner-private")?;
    let Some(saved) = load(&home.join(FILE))? else {
        println!("No hosted registration is saved in this hub home.");
        return Ok(());
    };
    let Some(binding) = saved.binding else {
        println!("Hosted setup has started. Run the same setup command to continue.");
        return Ok(());
    };
    let origin = https_origin(&saved.origin)?;
    let status =
        hosted_registration::status(origin.as_str(), &binding.installation_id, &saved.proof);
    let phase = match status {
        Ok(status) if status.public_url == binding.public_url => {
            status.lifecycle.unwrap_or(status.state)
        }
        Ok(_) => return Err("hosted enrollment binding changed".into()),
        Err(reason) if reason == "registration_expired" => "approval_expired".into(),
        Err(reason) if reason == "registration_revoked" => "revoked".into(),
        Err(reason) => return Err(reason),
    };
    println!("{}", lifecycle_notice(&phase));
    Ok(())
}

pub(crate) fn saved_route_provider(home: &Path, route: &str) -> Result<Option<Url>, String> {
    let Some(saved) = load(&home.join(FILE))? else {
        return Ok(None);
    };
    if saved.requested_routes.is_none()
        || saved
            .unix_descriptor
            .as_ref()
            .is_some_and(|scope| scope.mcp_paths.iter().any(|path| path == route))
    {
        return Ok(None);
    }
    https_origin(&saved.origin).map(Some)
}

fn registration_origin() -> Result<Url, String> {
    let configured = std::env::var_os("RADIO999K_REGISTRATION_URL")
        .or_else(|| std::env::var_os("PIGEONMQ_REGISTRATION_URL"))
        .or_else(|| (!RELEASE_ORIGIN.is_empty()).then(|| RELEASE_ORIGIN.into()))
        .ok_or("This build has no hosted registration provider. Use an operator-configured build or set RADIO999K_REGISTRATION_URL to your operator’s HTTPS origin.")?
        .into_string()
        .map_err(|_| "registration origin must be an HTTPS origin")?;
    https_origin(&configured)
}

fn reserve(
    home: &Path,
    origin: &Url,
    intent: OriginIntent,
    selected_mcp_path: Option<&str>,
) -> Result<String, String> {
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
    let selectable = intent == OriginIntent::SelectableUnix;
    let schedule = intent == OriginIntent::UnixV1 || selected_mcp_path.is_some();
    let public_url = enroll(
        &home,
        origin,
        origin.as_str(),
        Duration::from_secs(90),
        Duration::from_secs(2),
        intent,
        selected_mcp_path,
        open_browser,
        |url, id, token| crate::hosted_connector::stage(&home, url, id, token),
    )?;
    if selected_mcp_path.is_none() {
        println!("reserved_address={public_url}");
        if selectable && staged_unix(&home)?.is_some_and(|(_, scope, _)| scope.mcp_paths.is_empty())
        {
            eprintln!("Address reserved. Select a peer to publish its route.");
        }
    } else {
        println!("Hosted address reserved.");
    }
    let scheduled = if schedule {
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
    if !scheduled && schedule {
        eprintln!("Activate the required managed profiles, then repeat hosted setup.");
    }
    if selected_mcp_path.is_some() && !scheduled {
        return Err("The selected route is staged. Start its managed profile and repeat selected-peer setup.".into());
    }
    Ok(public_url)
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
    if paths.is_empty() {
        saved.requested_routes = Some(Vec::new());
    }
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
                    let mut expected = json!({"kind":"unix_v1", "nonce":saved.unix_nonce});
                    if let Some(routes) = saved.requested_routes {
                        expected["routes"] = json!(routes);
                    }
                    assert_eq!(body, expected);
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
            None,
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
                None,
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
    fn selected_enrollment_never_finalizes_a_wider_provider_scope() {
        let home = temp_home();
        let path = home.path().join(FILE);
        let origin = https_origin(ORIGIN).unwrap();
        let selected = "/muse/mcp";
        let (drop_api, dropped) = operator_with_intent(home.path(), vec![(BEGIN, None)], true);
        let run = |api: &str, finalize: fn(&str, &str, &str) -> Result<(), String>| {
            enroll(
                home.path(),
                &origin,
                api,
                Duration::ZERO,
                Duration::ZERO,
                OriginIntent::UnixV1,
                Some(selected),
                |_| panic!("unexpected opener"),
                finalize,
            )
        };
        assert!(run(&drop_api, |_, _, _| panic!("unexpected finalization")).is_err());
        dropped.join().unwrap();
        let pending = load(&path).unwrap().unwrap();
        assert_eq!(pending.selected_mcp_path.as_deref(), Some(selected));
        let nonce = pending.unix_nonce.as_deref().unwrap();
        let finish =
            "POST /v1/enrollments/11111111-1111-4111-8111-111111111111/finish-unix HTTP/1.1";
        let complete = |paths: &[&str]| {
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
                        json!({
                            "state":"ready", "public_url":PUBLIC, "connector_token":"fixture-token",
                            "origin":{"kind":"unix_v1","nonce":nonce,"hub_port":54843,"mcp_paths":paths}
                        }),
                    )),
                ),
            ]
        };
        let (api, server) =
            operator_with_intent(home.path(), complete(&[selected, "/cue/mcp"]), true);
        let error = run(&api, |_, _, _| {
            panic!("broader ingress must not reach finalization")
        })
        .unwrap_err();
        assert!(error.contains("did not approve exactly the selected peer route"));
        server.join().unwrap();
        assert!(load(&path).unwrap().unwrap().unix_descriptor.is_none());

        let (api, server) = operator_with_intent(home.path(), complete(&[selected]), true);
        assert_eq!(run(&api, |_, _, _| Ok(())).unwrap(), PUBLIC);
        server.join().unwrap();
        assert_eq!(
            load(&path)
                .unwrap()
                .unwrap()
                .unix_descriptor
                .unwrap()
                .mcp_paths,
            [selected]
        );
    }

    #[test]
    fn selectable_enrollment_reuses_saved_origin_and_rejects_extra_routes() {
        let home = temp_home();
        let path = home.path().join(FILE);
        let origin = https_origin(ORIGIN).unwrap();
        let mut saved = new_enrollment(&origin);
        let nonce = hosted_origin_local::new_origin().unwrap();
        saved.unix_nonce = Some(nonce.nonce().into());
        saved.requested_routes = Some(Vec::new());
        save(&path, &saved).unwrap();
        let steps = |paths: &[&str]| {
            vec![
                (BEGIN, Some((200, begin(ORIGIN, ID, None)))),
                (
                    STATUS,
                    Some((200, json!({"state":"ready","public_url":PUBLIC}))),
                ),
                (
                    "POST /v1/enrollments/11111111-1111-4111-8111-111111111111/finish-unix HTTP/1.1",
                    Some((
                        200,
                        json!({
                            "state":"ready", "public_url":PUBLIC, "connector_token":"fixture-token",
                            "origin":{"kind":"unix_v1","nonce":nonce.nonce(),"hub_port":54843,"mcp_paths":paths}
                        }),
                    )),
                ),
            ]
        };
        for (selected, paths, accepted) in [
            (None, &[][..], true),
            (Some("/cue/mcp"), &["/cue/mcp"][..], true),
            (
                Some("/muse/mcp"),
                &["/cue/mcp", "/muse/mcp", "/instinct/mcp"][..],
                false,
            ),
            (Some("/muse/mcp"), &["/cue/mcp", "/muse/mcp"][..], true),
        ] {
            if let Some(route) = selected {
                assert_eq!(
                    saved_route_provider(home.path(), route).unwrap(),
                    Some(origin.clone())
                );
            }
            let (api, server) = operator_with_intent(home.path(), steps(paths), true);
            let result = enroll(
                home.path(),
                &origin,
                &api,
                Duration::ZERO,
                Duration::ZERO,
                OriginIntent::SelectableUnix,
                selected,
                |_| panic!("the installation is already approved"),
                |url, id, _| {
                    assert!(accepted);
                    assert_eq!((url, id), (PUBLIC, ID));
                    Ok(())
                },
            );
            assert_eq!(result.is_ok(), accepted);
            server.join().unwrap();
        }
        let current = load(&path).unwrap().unwrap();
        assert_eq!(current.request_id, saved.request_id);
        assert_eq!(
            current.unix_descriptor.unwrap().mcp_paths,
            ["/cue/mcp", "/muse/mcp"]
        );
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
