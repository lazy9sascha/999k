//! Attach a separate, peer-scoped PigeonMQ tool to each local Orca relay bot.
use crate::{cloud_setup, stdio_mcp};
use pigeonmq::private_file;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::io::{self, IsTerminal, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const PEERS: [(&str, &str); 5] = [
    ("dot", "Dot"),
    ("instinct", "Instinct"),
    ("cue", "Cue"),
    ("muse", "Muse"),
    ("grok-bot", "GrokBot"),
];
const TOOL: &str = "pigeonmq";
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
        let file = home.join("pigeonmq-cloud-codex-header");
        if file.is_file() {
            return Ok(file);
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
    id: String,
    name: String,
    #[serde(default)]
    specialty: Option<String>,
    server: Option<Value>,
}

#[derive(Deserialize)]
struct Tool {
    name: String,
    remote: bool,
    detail: String,
}

#[derive(Deserialize)]
struct Inventory {
    own: Vec<Tool>,
    inherited: Vec<Value>,
}

fn purpose(peer: &str) -> String {
    format!(
        "PigeonMQ relay for {peer}. Use the local pigeonmq tool to send only to {peer}. Report Queued after acceptance. Wait on the original ID, and return the matching peer reply to its originating chat."
    )
}

fn relay_matches(bot: &Bot, peer: &str) -> bool {
    let description = bot.specialty.as_deref().unwrap_or_default().to_lowercase();
    description.contains("pigeonmq")
        && description
            .split(|c: char| !c.is_alphanumeric() && c != '-')
            .any(|s| s == peer)
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
fn attach(bot: &Bot, command: &str, run: &impl Fn(&[&str]) -> Result<Value>) -> Result<()> {
    let inventory = || -> Result<Inventory> {
        Ok(serde_json::from_value(run(&[
            "bot", "tools", "--bot", &bot.id,
        ])?)?)
    };
    let initial = inventory()?;
    let matches = |tools: &Inventory| {
        let mut ours = tools.own.iter().filter(|entry| entry.name == TOOL);
        ours.next()
            .is_some_and(|entry| !entry.remote && entry.detail == command)
            && ours.next().is_none()
    };
    if initial
        .inherited
        .iter()
        .any(|entry| entry["name"] == TOOL || entry == TOOL)
    {
        return Err(format!(
            "{} inherits a tool named pigeonmq. Resolve the name conflict in Orca.",
            bot.name
        )
        .into());
    }
    if matches(&initial) {
        return Ok(());
    }
    if initial.own.iter().any(|entry| entry.name == TOOL) {
        return Err(format!(
            "{} already has a different pigeonmq tool. Review it in Orca.",
            bot.name
        )
        .into());
    }
    run(&[
        "bot", "tools", "add", command, "--name", TOOL, "--bot", &bot.id,
    ])?;
    if !matches(&inventory()?) {
        return Err("Orca did not list the expected peer-scoped command after adding it.".into());
    }
    Ok(())
}

struct Setup<'a> {
    executable: &'a str,
    home: &'a str,
    sender: &'a str,
    bearer: &'a str,
    peers: Vec<(&'static str, &'static str)>,
}

impl Setup<'_> {
    fn resume_command(&self) -> String {
        format!(
            "{} init --orca --home {} --orca-sender {} --orca-bearer-file {}",
            self.executable, self.home, self.sender, self.bearer
        )
    }

    fn pass(
        &self,
        state_path: &Path,
        retry: Option<&str>,
        run: &impl Fn(&[&str]) -> Result<Value>,
    ) -> Result<(bool, String)> {
        let _lock = setup_lock(state_path)?;
        let mut pending: Pending = read_private_json(state_path)?.unwrap_or_default();
        let mut bots: Vec<Bot> = serde_json::from_value(run(&["bot", "list"])?["bots"].clone())?;
        bots.retain(|bot| bot.server.is_none());
        if let Some(peer) = retry {
            if !self.peers.iter().any(|(id, _)| *id == peer) {
                return Err("Unknown or unauthorized retry peer".into());
            }
            pending.offers.remove(peer);
            pending.handoff = None;
            pending.save(state_path)?;
        }

        let mut missing = Vec::new();
        let mut configured = Vec::new();
        let mut relay = None;
        for &(peer, name) in &self.peers {
            let mut candidates = bots
                .iter()
                .filter(|bot| bot.name.eq_ignore_ascii_case(name));
            let Some(bot) = candidates.next() else {
                missing.push((peer, name));
                continue;
            };
            if candidates.next().is_some() || !relay_matches(bot, peer) {
                return Err(format!("Review the existing {name} bot in Orca. Its identity must designate the PigeonMQ relay for {peer}.").into());
            }
            let command = format!(
                "{} stdio-mcp --home {} --mailbox {} --bearer-file {} --peer {peer}",
                self.executable, self.home, self.sender, self.bearer
            );
            attach(bot, &command, run)?;
            if pending.offers.remove(peer) {
                pending.save(state_path)?;
            }
            relay.get_or_insert(name);
            configured.push(name);
        }
        let summary = format!(
            "Configured {}/{}: {}",
            configured.len(),
            self.peers.len(),
            configured.join(", ")
        );
        if missing.is_empty() {
            if pending.handoff.take().is_some() {
                pending.save(state_path)?;
            }
            return Ok((true, summary));
        }
        let (first, first_name) = missing[0];
        if bots.is_empty() {
            return Ok((
                false,
                format!(
                    "{summary}\nOrca needs its first bot. Open Bots → New bot, name it {first_name}, set its identity to the following text and select Create with your provider:\n{}\nLeave this setup terminal open. For a noninteractive run, resume from that bot's Home Chat with: {}",
                    purpose(first),
                    self.resume_command()
                ),
            ));
        }
        let missing_names = missing
            .iter()
            .map(|(_, n)| *n)
            .collect::<Vec<_>>()
            .join(", ");
        for (peer, name) in missing {
            if pending.offers.contains(peer) {
                continue;
            }
            // If the caller is a plain setup terminal, use an already verified
            // relay's real Home Chat for native offers. Never forge a caller ID.
            if pending.handoff.is_some() && std::env::var_os("ORCA_AGENT_SESSION_ID").is_none() {
                break;
            }
            pending.offers.insert(peer.to_owned());
            pending.save(state_path)?;
            let result = run(&[
                "bot",
                "create",
                "--offer",
                "--name",
                name,
                "--purpose",
                &purpose(peer),
            ]);
            if let Err(err) = result {
                if err.to_string() != BOT_CHAT_REQUIRED {
                    return Err(format!("{err} Inspect the approval card for {name}. If none exists, repeat with --retry-orca-offer {peer}.").into());
                }
                // A wrong-context rejection cannot have offered a card.
                pending.offers.remove(peer);
                pending.save(state_path)?;
                if let Some(bot_name) = relay {
                    if pending.handoff.is_none() {
                        let prompt = format!(
                            "The owner started local PigeonMQ setup. Run this setup command once in this Home Chat to propose missing relay bots for owner approval: {} . Do not send any peer messages.",
                            self.resume_command()
                        );
                        pending.handoff = Some(bot_name.to_owned());
                        pending.save(state_path)?;
                        if run(&[
                            "orchestration",
                            "send",
                            "--to",
                            &format!("bot:{bot_name}"),
                            "--type",
                            "handoff",
                            "--subject",
                            "Set up PigeonMQ relay bots",
                            "--body",
                            &prompt,
                        ])
                        .is_err()
                        {
                            return Ok((
                                false,
                                format!(
                                    "{summary}\nIn {bot_name}'s Home Chat, ask it to run: {}",
                                    self.resume_command()
                                ),
                            ));
                        }
                    }
                } else {
                    return Ok((
                        false,
                        format!(
                            "{summary}\nIn an existing Orca bot's Home Chat, ask it to run: {}",
                            self.resume_command()
                        ),
                    ));
                }
                break;
            }
        }
        if let Some(bot_name) = &pending.handoff
            && pending.offers.is_empty()
        {
            return Ok((
                false,
                format!(
                    "{summary}\nWaiting for {bot_name}'s setup turn. If no approval cards appear, ask its Home Chat to run: {}",
                    self.resume_command()
                ),
            ));
        }
        Ok((
            false,
            format!(
                "{summary}\nWaiting for owner-approved bots: {missing_names}. Check the relay Home Chat for Create bot cards. If a card was dismissed, use --retry-orca-offer <peer> after checking Orca."
            ),
        ))
    }
}

pub(crate) fn onboard(
    home: &Path,
    sender: &str,
    bearer_file: &Path,
    retry: Option<&str>,
) -> Result<()> {
    if sender.is_empty()
        || !sender
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err("Invalid sender mailbox".into());
    }
    let home = cloud_setup::require_home(home)?;
    let executable = std::env::current_exe()?;
    let available = stdio_mcp::orca_recipients(&home, sender, bearer_file)?;
    let peers: Vec<_> = PEERS
        .into_iter()
        .filter(|(id, _)| available.iter().any(|s| s == id))
        .collect();
    if peers.is_empty() {
        return Err("No supported Orca peers were found for this sender.".into());
    }
    let setup = Setup {
        executable: path_arg(&executable)?,
        home: path_arg(&home)?,
        sender,
        bearer: path_arg(bearer_file)?,
        peers,
    };
    let state_path = home.join("orca-pending-offers.json");
    let wait = io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var_os("ORCA_AGENT_SESSION_ID").is_none();
    let mut previous = String::new();
    let mut retry = retry;
    loop {
        let (complete, status) = setup.pass(&state_path, retry.take(), &orca)?;
        if previous != status {
            println!("{status}");
            previous = status;
        }
        if complete || !wait {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
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
            executable: "/bin/pigeonmq",
            home: "/fixture",
            sender: "codex-cloud",
            bearer: "/fixture/header",
            peers: vec![("dot", "Dot"), ("cue", "Cue"), ("muse", "Muse")],
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
        let (done, summary) = setup().pass(&state, None, &run).unwrap();
        assert!(!done);
        assert!(summary.contains("New bot"));
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
                    if *name == "Cue" {
                        return Err("unknown receipt".into());
                    }
                    Ok(json!({"message":"card offered"}))
                }
                _ => panic!("unexpected operation"),
            }
        };
        assert!(setup().pass(&state, None, &run).is_err());
        assert_eq!(*cards.borrow(), ["Dot", "Cue"]);
        assert!(!setup().pass(&state, None, &run).unwrap().0);
        assert_eq!(*cards.borrow(), ["Dot", "Cue", "Muse"]);
        assert!(setup().pass(&state, Some("cue"), &run).is_err());
        assert_eq!(*cards.borrow(), ["Dot", "Cue", "Muse", "Cue"]);
    }

    #[test]
    fn existing_tool_is_reused_only_for_the_same_peer_command() {
        let bot = Bot {
            id: "local-id".into(),
            name: "Dot".into(),
            specialty: Some(purpose("dot")),
            server: None,
        };
        let run = |args: &[&str]| -> Result<Value> {
            assert_eq!(args, ["bot", "tools", "--bot", "local-id"]);
            Ok(
                json!({"own":[{"name":"pigeonmq","remote":false,"detail":"bin --peer dot"}],"inherited":[]}),
            )
        };
        attach(&bot, "bin --peer dot", &run).unwrap();
        assert!(attach(&bot, "bin --peer cue", &run).is_err());
        assert!(relay_matches(&bot, "dot"));
        assert!(!relay_matches(&bot, "cue"));
    }

    #[test]
    fn corrupted_receipt_prevents_another_offer() {
        let home = scratch();
        let state = home.path().join("pending.json");
        private_file::replace_private(&state, b"{broken").unwrap();
        let run = |_args: &[&str]| -> Result<Value> {
            panic!("must stop before contacting Orca");
        };
        assert!(setup().pass(&state, None, &run).is_err());
    }
}
