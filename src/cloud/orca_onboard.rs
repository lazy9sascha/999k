//! Attach a separate, peer-scoped 999K tool to each local Orca relay bot.
use crate::{cloud_setup, stdio_mcp};
#[path = "orca_remote.rs"]
mod remote;
use radio999k::private_file;
pub(crate) use remote::{Options as RemoteOptions, verified_pairing_origin};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const PEERS: [(&str, &str); 5] = [
    ("dot", "Dot"),
    ("instinct", "Instinct"),
    ("cue", "Cue"),
    ("muse", "Muse"),
    ("grok-bot", "GrokBot"),
];
const TOOL: &str = "p-999k";
const PREVIOUS_TOOL: &str = "radio999k";
const LEGACY_TOOL: &str = "pigeonmq";
const BOT_CHAT_REQUIRED: &str = "Orca requires a bot-owned chat to propose new bots";
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) fn resolve_bearer_file(home: &Path, sender: &str) -> Result<PathBuf> {
    let state = home.join(".pigeonmq-lifecycle/profiles.json");
    if let Some(state) = read_private_json::<Value>(&state)?
        && let Some(file) = state["profiles"][sender]["token_file"].as_str()
    {
        return Ok(PathBuf::from(file));
    }
    if sender == "codex-cloud" {
        // Existing installations can use a header exported from Keychain.
        let file = home.join("pigeonmq-cloud-codex-header");
        if file.is_file() {
            return Ok(file);
        }
        let token = home.join("pigeonmq-cloud-codex-cloud-token");
        if home.join(".pigeonmq-lifecycle/profiles.json").is_file() && token.try_exists()? {
            private_file::open_private_file(&token)
                .map_err(|_| "Invalid private sender credential")?;
            return Ok(token);
        }
    }
    Err("Pass --orca-bearer-file with the existing sender's private bearer file path.".into())
}

fn read_private_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match path.symlink_metadata() {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => {
            result?;
        }
    }
    let mut bytes = Vec::new();
    private_file::open_private_file(path)
        .map_err(|_| "Owner-private Orca setup or managed profile file required")?
        .take(private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err("Setup state is too large".into());
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[derive(Default, Serialize, Deserialize)]
struct Pending {
    // Offers have no supported inventory query. Reserve before invoking Orca so a
    // lost response cannot make the next run submit a second approval card.
    offers: BTreeSet<String>,
    handoff: Option<String>,
}

impl Pending {
    fn save(&self, path: &Path) -> Result<()> {
        Ok(private_file::replace_private(
            path,
            &serde_json::to_vec(self)?,
        )?)
    }
}

// Both the setup terminal and the bot that submits the cards can reconcile.
// Hold the lock for one pass only; never hold it while waiting for the owner.
fn setup_lock(state: &Path) -> Result<std::fs::File> {
    let path = state.with_extension("lock");
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(_) => (),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => (),
        Err(err) => return Err(err.into()),
    }
    let file =
        private_file::open_private_file(&path).map_err(|_| "Invalid private Orca setup lock")?;
    file.lock()?;
    Ok(file)
}

#[derive(Deserialize)]
struct Bot {
    #[allow(dead_code)]
    id: String,
    name: String,
    #[serde(default)]
    specialty: Option<String>,
    server: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Tool {
    name: String,
    remote: bool,
    detail: String,
    sign_in: Option<Value>,
}

#[derive(Deserialize)]
struct Inventory {
    own: Vec<Tool>,
    inherited: Vec<Value>,
}

fn purpose(peer: &str) -> String {
    format!(
        "999K relay for {peer}. Use the local peer-scoped MCP connector to send only to {peer}. Report Queued after acceptance. Wait with the original ID, and return the matching peer reply to its originating chat."
    )
}

fn relay_matches(bot: &Bot, peer: &str) -> bool {
    let description = bot.specialty.as_deref().unwrap_or_default().to_lowercase();
    let identity = description.split('.').next().unwrap_or_default().trim();
    ["999k", "p-999k", "pigeonmq"]
        .iter()
        .any(|brand| identity == format!("{brand} relay for {peer}"))
}

fn path_arg(path: &Path) -> Result<&str> {
    let value = path.to_str().ok_or("Orca setup requires UTF-8 paths")?;
    // Orca records a single space-joined command without argv boundaries.
    // Require unambiguous paths when comparing an existing tool with our binding.
    if !path.is_absolute()
        || !value
            .chars()
            .all(|c| c.is_alphanumeric() || "/._-+:@".contains(c))
    {
        return Err("Use absolute executable, home and bearer paths without spaces or shell metacharacters.".into());
    }
    Ok(value)
}

fn orca(args: &[&str]) -> Result<Value> {
    let invoke = |path: &Path| {
        Command::new(path)
            .args(args)
            .args(["--environment", "local", "--json"])
            .output()
    };
    let output = invoke(Path::new("orca"));
    #[cfg(target_os = "macos")]
    let output = output.or_else(|err| {
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
        let user_apps =
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Applications"));
        let cli = user_apps
            .into_iter()
            .chain([PathBuf::from("/Applications")])
            .map(|apps| apps.join("Orca.app/Contents/Resources/bin/orca"))
            .find(|path| path.is_file())
            .ok_or(err)?;
        invoke(&cli)
    });
    let output = output.map_err(|_| "Cannot run the local Orca CLI. Install it and check PATH.")?;
    let receipt: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| "Orca returned no valid receipt. Check the local Orca app.")?;
    if output.status.success() && receipt["ok"] == true {
        return receipt
            .get("result")
            .cloned()
            .ok_or_else(|| "Orca receipt has no result".into());
    }
    if receipt["error"]["code"] == "invalid_argument"
        && receipt["error"]["message"].as_str().is_some_and(|s| {
            s.to_ascii_lowercase().contains("bot's chat") || s.contains("bot’s chat")
        })
    {
        return Err(BOT_CHAT_REQUIRED.into());
    }
    // Even a failed command may include environment data in its diagnostic.
    Err("Orca did not confirm the requested action. Check its local bot or approval UI.".into())
}

// A successful result includes the expected configured command, including its peer.
fn attach(
    bot: &Bot,
    peer: &str,
    command: &str,
    run: &impl Fn(&[&str]) -> Result<Value>,
) -> Result<()> {
    let inventory = || -> Result<Inventory> {
        Ok(serde_json::from_value(run(&[
            "bot", "tools", "--bot", &bot.name,
        ])?)?)
    };
    let initial = inventory()?;
    let legacy_command = command.split_once(' ').and_then(|(path, args)| {
        let path = Path::new(path);
        (path.file_name()? == "999k").then(|| {
            let alias = path.with_file_name("pigeonmq");
            format!("{} {args}", alias.display())
        })
    });
    let inherits = |tools: &Inventory, name: &str| {
        tools
            .inherited
            .iter()
            .any(|entry| entry["name"] == name || entry == name)
    };
    let matches = |tools: &Inventory| {
        let mut ours = tools
            .own
            .iter()
            .filter(|entry| matches!(entry.name.as_str(), TOOL | PREVIOUS_TOOL | LEGACY_TOOL));
        ours.next().is_some_and(|entry| {
            let expected = match entry.name.as_str() {
                TOOL | PREVIOUS_TOOL => entry.detail == command,
                LEGACY_TOOL => {
                    entry.detail == command
                        || legacy_command.as_deref() == Some(entry.detail.as_str())
                }
                _ => false,
            };
            !entry.remote && entry.sign_in.is_none() && expected && !inherits(tools, &entry.name)
        }) && ours.next().is_none()
    };
    if matches(&initial) {
        return Ok(());
    }
    if !relay_matches(bot, peer) {
        return Err(format!(
            "Review the existing {} bot in Orca. To use it as this relay, set its identity to: {}",
            bot.name,
            purpose(peer)
        )
        .into());
    }
    if inherits(&initial, TOOL) {
        return Err(format!(
            "{} inherits a tool named {TOOL}. Resolve the name conflict in Orca.",
            bot.name
        )
        .into());
    }
    if initial
        .own
        .iter()
        .any(|entry| matches!(entry.name.as_str(), TOOL | PREVIOUS_TOOL | LEGACY_TOOL))
    {
        return Err(format!(
            "{} already has a different 999K relay tool. Review it in Orca.",
            bot.name
        )
        .into());
    }
    run(&[
        "bot", "tools", "add", command, "--name", TOOL, "--bot", &bot.name,
    ])?;
    let final_inventory = inventory()?;
    if !matches(&final_inventory) || !final_inventory.own.iter().any(|entry| entry.name == TOOL) {
        return Err("Orca did not list the expected peer-scoped command after adding it.".into());
    }
    Ok(())
}

struct Setup<'a> {
    executable: &'a str,
    home: &'a str,
    sender: &'a str,
    bearer: &'a str,
    peer: (&'static str, &'static str),
    wait_for_approval: bool,
}

impl Setup<'_> {
    fn resume_command(&self) -> String {
        format!(
            "{} init --orca --orca-peer {} --home {} --orca-sender {} --orca-bearer-file {}",
            self.executable, self.peer.0, self.home, self.sender, self.bearer
        )
    }

    fn pass(
        &self,
        state_path: &Path,
        retry: bool,
        run: &impl Fn(&[&str]) -> Result<Value>,
    ) -> Result<(bool, String)> {
        let (peer, name) = self.peer;
        let _lock = setup_lock(state_path)?;
        let mut pending: Pending = read_private_json(state_path)?.unwrap_or_default();
        let bots: Vec<Bot> = serde_json::from_value(run(&["bot", "list"])?["bots"].clone())?;
        let mut local = bots.iter().filter(|bot| bot.server.is_none());
        let mut candidates = local
            .clone()
            .filter(|bot| bot.name.eq_ignore_ascii_case(name));
        if let Some(bot) = candidates.next() {
            if candidates.next().is_some() {
                return Err(format!(
                    "Multiple local bots have the name {name}. Choose a unique relay name in Orca."
                )
                .into());
            }
            let command = format!(
                "{} stdio-mcp --home {} --mailbox {} --bearer-file {} --peer {peer}",
                self.executable, self.home, self.sender, self.bearer
            );
            attach(bot, peer, &command, run)?;
            if pending.offers.remove(peer) {
                pending.save(state_path)?;
            }
            return Ok((true, format!("Local relay for {name}: bound in Orca.")));
        }

        if retry && pending.offers.remove(peer) {
            pending.save(state_path)?;
        }
        if local.next().is_none() {
            let next = if self.wait_for_approval {
                "Keep this terminal open. Setup continues after you create the bot.".to_owned()
            } else {
                format!(
                    "After creation, run setup again:\n{}",
                    self.resume_command()
                )
            };
            return Ok((
                false,
                format!(
                    "Open Bots → New bot. Choose your agent and select Create.\n\nName: {name}\nIdentity: {}\n\n{next}",
                    purpose(peer)
                ),
            ));
        }

        if !pending.offers.contains(peer) {
            pending.offers.insert(peer.to_owned());
            pending.save(state_path)?;
            if let Err(err) = run(&[
                "bot",
                "create",
                "--offer",
                "--name",
                name,
                "--purpose",
                &purpose(peer),
            ]) {
                if err.to_string() != BOT_CHAT_REQUIRED {
                    return Err(format!("{err} Inspect the approval card for {name}. If none exists, repeat with --retry-orca-offer {peer}.").into());
                }
                // A wrong-context rejection cannot have offered a card.
                pending.offers.remove(peer);
                pending.save(state_path)?;
                return Ok((
                    false,
                    format!(
                        "In an existing Orca bot's Home Chat, ask it to run: {}",
                        self.resume_command()
                    ),
                ));
            }
        }

        Ok((
            false,
            format!(
                "Waiting for owner approval for {name}. If its Create bot card was dismissed, repeat with --retry-orca-offer {peer} after checking Orca."
            ),
        ))
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PeerAction {
    List(Vec<&'static str>),
    SetUp((&'static str, &'static str)),
}

fn authorized_peer(
    selected: Option<&str>,
    may_provision: bool,
    mut discover: impl FnMut() -> io::Result<Vec<String>>,
    provision: impl FnOnce(&str) -> io::Result<()>,
) -> Result<PeerAction> {
    let available = discover()?;
    let Some(selected) = selected else {
        let peers: Vec<_> = PEERS
            .iter()
            .filter_map(|(id, _)| available.iter().any(|peer| peer == id).then_some(*id))
            .collect();
        return Ok(PeerAction::List(peers));
    };
    let peer = PEERS
        .iter()
        .copied()
        .find(|(id, _)| *id == selected)
        .ok_or("Unsupported Orca peer")?;
    if !available.iter().any(|id| id == selected) {
        if !may_provision {
            return Err(format!("Orca sender is not authorized for {selected}.").into());
        }
        provision(selected)?;
        if !discover()?.iter().any(|id| id == selected) {
            return Err(format!(
                "Orca sender is still not authorized for {selected} after local setup."
            )
            .into());
        }
    }
    Ok(PeerAction::SetUp(peer))
}

pub(crate) struct OnboardOptions<'a> {
    pub(crate) selected: Option<&'a str>,
    pub(crate) retry: bool,
    pub(crate) may_provision: bool,
    pub(crate) remote: RemoteOptions<'a>,
    pub(crate) proof_original: Option<&'a str>,
    pub(crate) email_recipient: Option<&'a str>,
    pub(crate) relay_command: Option<&'a Path>,
}

pub(crate) fn onboard(
    home: &Path,
    sender: &str,
    bearer_file: &Path,
    options: OnboardOptions<'_>,
) -> Result<()> {
    let OnboardOptions {
        selected,
        retry,
        may_provision,
        remote,
        proof_original,
        email_recipient,
        relay_command,
    } = options;
    if email_recipient.is_some() && !matches!(selected, Some("cue" | "instinct")) {
        return Err("Email arrival setup is available for Cue and Instinct only.".into());
    }
    if sender.is_empty()
        || !sender
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err("Invalid sender mailbox".into());
    }
    if email_recipient.is_some() && (!may_provision || retry) {
        return Err("Configure email arrivals with the default managed local sender.".into());
    }
    let home = cloud_setup::require_home(home)?;
    let mut configured_email = false;
    let peer = match authorized_peer(
        selected,
        may_provision && !retry,
        || stdio_mcp::orca_recipients(&home, sender, bearer_file),
        |name| {
            match email_recipient {
                Some(recipient) => {
                    let result = crate::lifecycle::add_orca_peer_with_email(
                        &home,
                        name,
                        recipient,
                        relay_command,
                    );
                    configured_email = result.is_ok();
                    result
                }
                None => crate::lifecycle::add_orca_peer(&home, name),
            }
            .map(|_| ())
        },
    )? {
        PeerAction::SetUp(peer) => peer,
        PeerAction::List(peers) => {
            if peers.is_empty() {
                println!("Authorized Orca peers: 0");
            } else {
                println!("Authorized Orca peers: {}", peers.join(", "));
            }
            return Ok(());
        }
    };
    if let Some(original) = proof_original {
        match stdio_mcp::orca_linked_reply(&home, sender, bearer_file, peer.0, original) {
            Ok(true) => {
                println!(
                    "Linked reply from {}: verified in the hub. Check the originating project chat for the answer.",
                    peer.0
                );
                return Ok(());
            }
            Ok(false) => {
                return Err(format!(
                    "Linked reply from {}: pending for the selected original request.",
                    peer.0
                )
                .into());
            }
            Err(_) => {
                return Err(format!(
                    "Linked reply from {}: could not verify the selected original request.",
                    peer.0
                )
                .into());
            }
        }
    }
    if let Some(recipient) = email_recipient
        && !configured_email
    {
        crate::lifecycle::add_orca_peer_with_email(&home, peer.0, recipient, relay_command)?;
    }
    remote::prepare(&home, peer.0, remote);
    print_email_status(&home, peer.0);
    println!(
        "Linked reply from {}: unverified. Complete a request and matching reply in the originating project chat.",
        peer.0
    );
    println!("{}", crate::orca_handoff::instructions(peer.0));
    let executable = std::env::current_exe()?;
    let wait_for_approval = io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var_os("ORCA_AGENT_SESSION_ID").is_none();
    let setup = Setup {
        executable: path_arg(&executable)?,
        home: path_arg(&home)?,
        sender,
        bearer: path_arg(bearer_file)?,
        peer,
        wait_for_approval,
    };
    let state_path = home.join("orca-pending-offers.json");
    let mut previous = String::new();
    let mut retry = retry;
    loop {
        let (complete, status) = setup.pass(&state_path, std::mem::take(&mut retry), &orca)?;
        if previous != status {
            println!("{status}");
            previous = status;
        }
        if complete || !setup.wait_for_approval {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

fn print_email_status(home: &Path, peer: &str) {
    if !matches!(peer, "cue" | "instinct") {
        return;
    }
    let path = home.join(".pigeonmq-lifecycle/relay-profiles.json");
    match read_private_json::<radio999k::arrival_notify::RelayConfig>(&path) {
        Ok(Some(config)) if config.contains_key(peer) => {
            println!("Email arrivals for {peer}: configured. Provider delivery unverified.");
        }
        Ok(_) if peer == "instinct" => {}
        Ok(_) => {
            println!(
                "Email arrivals for {peer}: waiting for a recipient and an approved relay command. Pass --orca-email-recipient and, if needed, --orca-relay-command to opt in."
            );
        }
        Err(_) => {
            println!("Email arrivals for {peer}: could not verify the private relay settings.");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::os::unix::fs::PermissionsExt;

    fn setup() -> Setup<'static> {
        Setup {
            executable: "/bin/999k",
            home: "/fixture",
            sender: "codex-cloud",
            bearer: "/fixture/header",
            wait_for_approval: true,
            peer: ("dot", "Dot"),
        }
    }

    fn scratch() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        dir
    }

    #[test]
    fn remote_roster_is_empty_locally_and_never_receives_an_offer() {
        let home = scratch();
        let state = home.path().join("pending.json");
        let run = |args: &[&str]| -> Result<Value> {
            assert_eq!(args, ["bot", "list"]);
            Ok(json!({"bots":[{"id":"remote","name":"Dot","server":{"name":"elsewhere"}}]}))
        };
        let (done, summary) = setup().pass(&state, false, &run).unwrap();
        assert!(!done);
        assert!(summary.contains("Name: Dot"));
        assert!(summary.contains("New bot"));
        assert!(!summary.contains("--orca-bearer-file"));
        let one_shot = Setup {
            wait_for_approval: false,
            ..setup()
        };
        let (_, summary) = one_shot.pass(&state, false, &run).unwrap();
        assert!(summary.contains(&one_shot.resume_command()));
        assert!(!state.exists());
    }

    #[test]
    fn ambiguous_offer_is_not_repeated_and_recovery_targets_one_peer() {
        let home = scratch();
        let state = home.path().join("pending.json");
        let cards = RefCell::new(Vec::new());
        let run = |args: &[&str]| -> Result<Value> {
            match args {
                ["bot", "list"] => Ok(json!({"bots":[{"id":"local","name":"Coordinator"}]})),
                ["bot", "create", "--offer", "--name", name, "--purpose", _] => {
                    cards.borrow_mut().push(name.to_string());
                    Err("unknown receipt".into())
                }
                _ => panic!("unexpected operation"),
            }
        };
        let initial = Pending {
            offers: BTreeSet::from(["cue".into()]),
            handoff: None,
        };
        initial.save(&state).unwrap();
        assert!(setup().pass(&state, false, &run).is_err());
        assert_eq!(*cards.borrow(), ["Dot"]);
        assert!(!setup().pass(&state, false, &run).unwrap().0);
        assert_eq!(*cards.borrow(), ["Dot"]);
        assert!(setup().pass(&state, true, &run).is_err());
        assert_eq!(*cards.borrow(), ["Dot", "Dot"]);
        assert_eq!(
            read_private_json::<Pending>(&state)
                .unwrap()
                .unwrap()
                .offers,
            BTreeSet::from(["cue".into(), "dot".into()]),
        )
    }

    #[test]
    fn existing_tool_is_reused_only_for_the_same_peer_command() {
        let bot = Bot {
            id: "local-id".into(),
            name: "Dot".into(),
            specialty: Some(purpose("dot")),
            server: None,
        };
        for (tool, path) in [
            (TOOL, "/bin/999k"),
            (PREVIOUS_TOOL, "/bin/999k"),
            (LEGACY_TOOL, "/bin/pigeonmq"),
        ] {
            let run = |args: &[&str]| -> Result<Value> {
                assert_eq!(args, ["bot", "tools", "--bot", "Dot"]);
                Ok(json!({"own":[{"name": tool, "remote": false,
                                  "detail": format!("{path} --peer dot")}],"inherited":[]}))
            };
            attach(&bot, "dot", "/bin/999k --peer dot", &run).unwrap();
            assert!(attach(&bot, "dot", "/bin/999k --peer cue", &run).is_err());
            assert!(attach(&bot, "dot", "/another/bin/999k --peer dot", &run).is_err());
        }
        assert!(relay_matches(&bot, "dot"));
        assert!(!relay_matches(&bot, "cue"));
        let unrelated = Bot {
            specialty: Some("Reviews 999K docs and asks Dot for help".into()),
            id: "unrelated".into(),
            name: "Dot".into(),
            server: None,
        };
        let list_only = |args: &[&str]| -> Result<Value> {
            assert_eq!(args, ["bot", "tools", "--bot", "Dot"]);
            Ok(json!({"own":[],"inherited":[]}))
        };
        assert!(attach(&unrelated, "dot", "/bin/999k --peer dot", &list_only).is_err());
        let old = Bot {
            specialty: Some("PigeonMQ relay for dot.".into()),
            ..bot
        };
        assert!(relay_matches(&old, "dot"));
        assert!(!relay_matches(&old, "cue"));
    }

    #[test]
    fn fresh_bots_attach_the_new_name_beside_an_inherited_legacy_tool() {
        let bot = Bot {
            id: "local-id".into(),
            name: "Dot".into(),
            specialty: Some(purpose("dot")),
            server: None,
        };
        let command = "/bin/999k stdio-mcp --peer dot";
        let added = std::cell::Cell::new(false);
        let run = |args: &[&str]| -> Result<Value> {
            match args {
                ["bot", "tools", "--bot", "Dot"] => {
                    let own = if added.get() {
                        vec![json!({"name": TOOL, "remote": false, "detail": command})]
                    } else {
                        vec![]
                    };
                    Ok(json!({"own": own, "inherited": [{"name": LEGACY_TOOL}]}))
                }
                [
                    "bot",
                    "tools",
                    "add",
                    detail,
                    "--name",
                    TOOL,
                    "--bot",
                    "Dot",
                ] => {
                    assert_eq!(*detail, command);
                    assert!(!added.replace(true));
                    Ok(Value::Null)
                }
                _ => panic!("unexpected operation"),
            }
        };
        attach(&bot, "dot", command, &run).unwrap();
        attach(&bot, "dot", command, &run).unwrap();
        assert!(added.get());
    }

    #[test]
    fn discovery_only_lists_authorized_supported_peers_without_provisioning() {
        for (available, expected) in [
            (vec!["other"], vec![]),
            (vec!["cue", "other", "dot"], vec!["dot", "cue"]),
        ] {
            let discovered = authorized_peer(
                None,
                true,
                || Ok(available.iter().map(|id| (*id).to_owned()).collect()),
                |_| panic!("discovery must not provision"),
            )
            .unwrap();
            assert_eq!(discovered, PeerAction::List(expected));
        }
    }

    #[test]
    fn authorization_fences_provisioning_and_requires_a_fresh_read() {
        let calls = RefCell::new(Vec::new());
        let already = authorized_peer(
            Some("cue"),
            true,
            || Ok(vec!["cue".into()]),
            |_| panic!("an already authorized peer must not be provisioned"),
        )
        .unwrap();
        assert_eq!(already, PeerAction::SetUp(("cue", "Cue")));
        assert!(
            authorized_peer(
                Some("cue"),
                false,
                || Ok(vec!["dot".into()]),
                |_| panic!("an alternate sender or retry must not provision")
            )
            .is_err()
        );
        assert!(
            authorized_peer(
                Some("cue"),
                true,
                || {
                    let mut calls = calls.borrow_mut();
                    calls.push("discover".to_owned());
                    Ok(vec!["dot".into()])
                },
                |id| {
                    calls.borrow_mut().push(id.to_owned());
                    Ok(())
                },
            )
            .is_err()
        );
        assert_eq!(*calls.borrow(), ["discover", "cue", "discover"]);
    }

    #[test]
    fn binding_touches_only_the_selected_local_bot() {
        let home = scratch();
        let state = home.path().join("pending.json");
        let selected = setup();
        let command = "/bin/999k stdio-mcp --home /fixture --mailbox codex-cloud --bearer-file /fixture/header --peer dot";
        let run = |args: &[&str]| -> Result<Value> {
            match args {
                ["bot", "list"] => Ok(json!({"bots":[
                    {"id":"dot","name":"Dot","specialty":purpose("dot")},
                    {"id":"cue","name":"Cue","specialty":purpose("cue")},
                ]})),
                ["bot", "tools", "--bot", "Dot"] => Ok(
                    json!({"own":[{"name":TOOL,"remote":false,"detail":command}],"inherited":[]}),
                ),
                _ => panic!("must not access the other bot"),
            }
        };
        let (done, summary) = selected.pass(&state, false, &run).unwrap();
        assert!(done);
        assert_eq!(summary, "Local relay for Dot: bound in Orca.");
        assert!(selected.resume_command().contains("--orca-peer dot"));
    }

    #[test]
    fn corrupted_receipt_prevents_another_offer() {
        let home = scratch();
        let state = home.path().join("pending.json");
        private_file::replace_private(&state, b"{broken").unwrap();
        let run = |_args: &[&str]| -> Result<Value> {
            panic!("must stop before contacting Orca");
        };
        assert!(setup().pass(&state, false, &run).is_err());
    }
}
