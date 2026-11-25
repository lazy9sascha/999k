//! Explicit owner offer and remote credential import.
use crate::{cloud_setup, cloud_transport, orca_onboard, pairing};
use radio999k::private_file;
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{self, IsTerminal, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(clap::Subcommand)]
pub(crate) enum Action {
    /// Display one offer code in the owner's local terminal.
    Offer {
        peer: String,
        /// Override the selected peer's verified HTTPS ingress.
        #[arg(long)]
        origin: Option<String>,
        #[arg(long)]
        home: Option<PathBuf>,
        /// Owner-private file with the currently active peer bearer.
        #[arg(long)]
        bearer_file: Option<PathBuf>,
    },
    /// Redeem one code and save the peer bearer in a new private file.
    ///
    /// The macOS client prompts with terminal echo off unless an input option
    /// is supplied. Use an input option on other platforms.
    Redeem {
        peer: String,
        /// The same HTTPS origin provided by the owner.
        #[arg(long)]
        origin: String,
        /// A new absolute file path in an existing owner-private directory.
        #[arg(long)]
        output: PathBuf,
        /// Read the code from an owner-private file.
        #[arg(long, conflicts_with = "code_stdin")]
        code_file: Option<PathBuf>,
        /// Read the code from a protected pipe.
        #[arg(long)]
        code_stdin: bool,
    },
}

fn routes(peer: &str, origin: &str) -> io::Result<(String, &'static str, &'static str)> {
    let origin = cloud_transport::public_origin(origin).map_err(|_| {
        io::Error::other("An HTTPS hub origin without a path or credentials is required")
    })?;
    let (mcp, pair) = pairing::route(peer).ok_or_else(|| {
        io::Error::other("Pairing is available for muse, grok-bot, cue, and instinct")
    })?;
    Ok((origin, mcp, pair))
}

fn read_code(reader: impl Read) -> io::Result<String> {
    let mut raw = String::new();
    reader
        .take(257)
        .read_to_string(&mut raw)
        .map_err(|_| io::Error::other("Could not read the pairing code"))?;
    let code = raw.trim_end_matches(['\r', '\n']);
    if code.is_empty() || raw.len() > 256 || !code.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(io::Error::other("Invalid pairing code input"));
    }
    Ok(code.to_owned())
}

fn code_input(path: Option<&Path>, stdin: bool) -> io::Result<String> {
    if let Some(path) = path {
        return read_code(
            private_file::open_private_file(path)
                .map_err(|_| io::Error::other("Owner-private code file required"))?,
        );
    }
    if stdin {
        if io::stdin().is_terminal() {
            return Err(io::Error::other(
                "Use the hidden prompt or a protected pipe",
            ));
        }
        return read_code(io::stdin().lock());
    }
    #[cfg(target_os = "macos")]
    {
        let code = crate::cloud_credentials::hidden_tty(c"Pairing code: ")?;
        read_code(code.as_bytes())
    }
    #[cfg(not(target_os = "macos"))]
    Err(io::Error::other(
        "Use --code-stdin with a protected pipe or --code-file on this platform",
    ))
}

#[derive(serde::Deserialize)]
struct Receipt {
    mailbox: String,
    mcp_path: String,
    token_type: String,
    access_token: String,
}

fn redeem(endpoint: &str, peer: &str, mcp: &str, code: &str) -> io::Result<String> {
    let client = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .no_proxy()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|_| io::Error::other("Could not start the HTTPS client"))?;
    let response = client
        .post(endpoint)
        .json(&serde_json::json!({"code": code}))
        .send()
        .map_err(|_| io::Error::other("Pairing request failed"))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(io::Error::other(
            "Pairing was refused by the selected HTTPS endpoint",
        ));
    }
    let mut body = Vec::new();
    response
        .take(4097)
        .read_to_end(&mut body)
        .map_err(|_| io::Error::other("Could not read the pairing receipt"))?;
    if body.len() > 4096 {
        return Err(io::Error::other("Pairing receipt is too large"));
    }
    validate_receipt(&body, peer, mcp)
}

fn validate_receipt(body: &[u8], peer: &str, mcp: &str) -> io::Result<String> {
    let receipt: Receipt =
        serde_json::from_slice(body).map_err(|_| io::Error::other("Invalid pairing receipt"))?;
    if receipt.mailbox != peer
        || receipt.mcp_path != mcp
        || receipt.token_type != "Bearer"
        || receipt.access_token.is_empty()
        || receipt.access_token.len() > 256
        || !receipt.access_token.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(io::Error::other(
            "Pairing receipt does not match the selected mailbox",
        ));
    }
    Ok(receipt.access_token)
}

fn save_new(path: &Path, get_token: impl FnOnce() -> io::Result<String>) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|_| path.is_absolute())
        .ok_or_else(|| io::Error::other("An absolute private destination is required"))?;
    let name = CString::new(
        path.file_name()
            .ok_or_else(|| io::Error::other("A bearer filename is required"))?
            .as_bytes(),
    )
    .map_err(|_| io::Error::other("Invalid bearer filename"))?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(parent)?;
    let meta = directory.metadata()?;
    if meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o077 != 0 {
        return Err(io::Error::other(
            "An owner-private destination directory is required",
        ));
    }
    // Reserve the new file before sending a request that can consume the code.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        let token = get_token()?;
        file.write_all(token.as_bytes())?;
        file.sync_all()
    })();
    if result.is_err() {
        // Only remove the entry that this invocation reserved.
        let _ = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) };
    }
    result?;
    directory.sync_all()
}

pub(crate) fn run(action: &Action) -> io::Result<()> {
    match action {
        Action::Offer {
            peer,
            origin,
            home,
            bearer_file,
        } => {
            if !io::stdin().is_terminal()
                || !io::stdout().is_terminal()
                || std::env::var_os("ORCA_AGENT_SESSION_ID").is_some()
            {
                return Err(io::Error::other(
                    "Pairing offers require the owner's local terminal",
                ));
            }
            let home = cloud_setup::resolve_home(home.clone());
            let bearer = match bearer_file {
                Some(file) => file.clone(),
                None => orca_onboard::resolve_bearer_file(&home, peer)
                    .map_err(|_| io::Error::other("Attach the peer's managed bearer or pass --bearer-file with its private file path"))?,
            };
            let origin = match origin {
                Some(explicit) => routes(peer, explicit)?.0,
                None => orca_onboard::verified_pairing_origin(&home, peer, &bearer)?,
            };
            let code = pairing::issue(&home, peer, &bearer)?;
            let mut stdout = io::stdout().lock();
            let (_, mcp, pair) = routes(peer, &origin)?;
            writeln!(stdout, "HTTPS hub origin: {origin}")?;
            writeln!(
                stdout,
                "Set this address in the receiving client's private connection settings."
            )?;
            writeln!(
                stdout,
                "Paste the following into the private chat of {peer}:"
            )?;
            if peer == "muse" {
                writeln!(
                    stdout,
                    "Set up the P-999K {peer} Streamable HTTP MCP connector in your runtime."
                )?;
            } else {
                writeln!(
                    stdout,
                    "Connect your existing {peer} app to its P-999K mailbox through authenticated Streamable HTTP MCP."
                )?;
            }
            writeln!(stdout, "Pairing code: {code}")?;
            writeln!(
                stdout,
                "Redeem this code once within 10 minutes. POST JSON with a single code field to {pair} on the approved origin in your connection settings. Require HTTPS, disable redirects and keep the response out of logs."
            )?;
            if peer == "muse" {
                writeln!(
                    stdout,
                    "Save the returned access_token directly in your private credential store for {mcp}. Set Authorization to Bearer with that value. Then initialize the connector and list its mailbox tools. Never print the token or return it in chat."
                )?;
            } else {
                writeln!(
                    stdout,
                    "Verify the response mailbox is {peer} and mcp_path is {mcp}, with a Bearer token type. Save access_token in private storage. Use it as the Authorization Bearer for MCP initialize and tools/list at {mcp}. Keep the token out of logs and chat."
                )?;
            }
            writeln!(stdout, "{}", crate::orca_handoff::intake(&peer))?;
            writeln!(stdout, "{}", crate::orca_handoff::VERIFY)?;
            Ok(())
        }
        Action::Redeem {
            peer,
            origin,
            output,
            code_file,
            code_stdin,
        } => {
            let (origin, mcp, pair) = routes(peer, origin)?;
            let endpoint = format!("{origin}{pair}");
            save_new(output, || {
                let code = code_input(code_file.as_deref(), *code_stdin)?;
                redeem(&endpoint, peer, mcp, &code)
            })?;
            println!("Saved the {peer} bearer in the requested private file.");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_are_pinned_to_https_and_the_selected_mailbox() {
        assert_eq!(
            routes("grok-bot", "https://pair.example/").unwrap(),
            ("https://pair.example".into(), "/grok/mcp", "/grok/pair")
        );
        for origin in [
            "http://pair.example",
            "https://@pair.example",
            "https://pair.example/./",
            "https://pair.example/muse/pair",
            "https://pair.example?code=fixture",
            "https://pair.example#fixture",
        ] {
            assert!(routes("muse", origin).is_err(), "{origin}");
        }
        assert!(routes("dot", "https://pair.example").is_err());
    }

    #[test]
    fn validates_receipt_against_the_selected_mailbox_and_route() {
        let mut receipt = serde_json::json!({
            "mailbox": "cue",
            "mcp_path": "/cue/mcp",
            "token_type": "Bearer",
            "access_token": "synthetic-token"
        });
        let body = receipt.to_string();
        assert_eq!(
            validate_receipt(body.as_bytes(), "cue", "/cue/mcp").unwrap(),
            "synthetic-token"
        );
        assert!(validate_receipt(body.as_bytes(), "muse", "/muse/mcp").is_err());
        receipt["access_token"] = serde_json::json!("bad token");
        assert!(validate_receipt(receipt.to_string().as_bytes(), "cue", "/cue/mcp").is_err());
    }

    #[test]
    fn destination_is_reserved_before_redemption_and_saved_at_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bearer");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(save_new(&path, || Err(io::Error::other("refused"))).is_err());
        assert!(!path.exists());
        save_new(&path, || Ok("synthetic-token".into())).unwrap();
        assert_eq!(path.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "synthetic-token");
        assert!(save_new(&path, || panic!("must not consume code")).is_err());
    }
}
