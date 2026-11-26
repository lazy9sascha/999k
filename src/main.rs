use radio999k::{mailbox_catalog as catalog, mcp_sdk};
mod auth;
#[path = "cloud/cloud_credentials.rs"]
mod cloud_credentials;
#[path = "cloud/cloud_service.rs"]
mod cloud_service;
#[path = "cloud/cloud_setup.rs"]
mod cloud_setup;
#[path = "cloud/cloud_transport.rs"]
mod cloud_transport;
#[cfg(any(target_os = "macos", test))]
#[path = "cloud/cloud_tunnel_management.rs"]
mod cloud_tunnel_management;
#[cfg(any(target_os = "macos", test))]
#[path = "cloud/cloud_tunnel_runtime.rs"]
mod cloud_tunnel_runtime;
#[path = "adapters/codex/events.rs"]
mod codex_events;
#[path = "adapters/codex/mailbox.rs"]
mod codex_mcp;
#[path = "adapters/codex/setup.rs"]
mod codex_setup;
#[path = "cloud/hosted_connector.rs"]
mod hosted_connector;
#[path = "cloud/hosted_enrollment.rs"]
mod hosted_enrollment;
#[path = "cloud/hosted_mailbox_bearer.rs"]
mod hosted_mailbox_bearer;
#[path = "cloud/hosted_origin.rs"]
mod hosted_origin;
#[path = "cloud/hosted_origin_local.rs"]
mod hosted_origin_local;
#[path = "cloud/hosted_registration.rs"]
mod hosted_registration;
#[path = "cloud/lifecycle.rs"]
mod lifecycle;
#[path = "cloud/lifecycle_profile.rs"]
mod lifecycle_profile;
#[path = "cloud/lifecycle_service.rs"]
mod lifecycle_service;
mod local_http;
#[path = "cloud/mailbox_startup.rs"]
mod mailbox_startup;
#[path = "cloud/orca_handoff.rs"]
mod orca_handoff;
#[path = "cloud/orca_onboard.rs"]
mod orca_onboard;
#[path = "cloud/pairing.rs"]
mod pairing;
#[path = "cloud/pairing_cli.rs"]
mod pairing_cli;
#[path = "cloud/registration_origin.rs"]
mod registration_origin;
#[path = "cloud/reply_mailbox_setup.rs"]
mod reply_mailbox_setup;
#[path = "cloud/runtime_profiles.rs"]
mod runtime_profiles;
#[path = "cloud/stdio_mcp.rs"]
mod stdio_mcp;

use clap::{Parser, Subcommand};
use radio999k::Queue;
use std::path::PathBuf;
use std::time::Duration;

const CODEX_MAILBOX: &str = "pigeonmq-cloud-codex";

#[derive(Subcommand)]
enum CodexQueueCommand {
    Claim,
    Complete,
}

#[derive(serde::Deserialize)]
struct CodexFinal {
    receipt: String,
    body: String,
}

#[derive(Parser)]
#[command(name = "999k", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct ServeHubOptions {
    #[arg(long, required_unless_present = "mailbox_home")]
    queue: Option<PathBuf>,
    /// Mount only the isolated mailbox profiles staged by init <agent>.
    #[arg(long, conflicts_with_all = ["queue", "auth_policy", "bundled_profiles", "continuous"])]
    mailbox_home: Option<PathBuf>,
    /// Load existing adapters and their bindings from an explicit private manifest.
    #[arg(long, conflicts_with_all = ["mailbox_home", "auth_policy", "bundled_profiles"])]
    profiles_file: Option<PathBuf>,
    #[arg(long, hide = true, requires = "profiles_file")]
    profiles_fingerprint: Option<String>,
    /// Internal supervisor mode: select the packaged bindings using the queue directory.
    #[arg(long, hide = true, requires = "queue")]
    bundled_profiles: bool,
    /// Enable generic HTTP and MCP routes with this private policy.
    #[arg(long)]
    auth_policy: Option<PathBuf>,
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,
    #[arg(long, required_unless_present = "continuous", conflicts_with = "continuous",
        value_parser = clap::value_parser!(u64).range(1..=3600))]
    seconds: Option<u64>,
    #[arg(long)]
    continuous: bool,
    /// Also bind an owner-private Unix socket for local MCP clients.
    #[arg(long)]
    stdio_socket: bool,
    #[arg(long, hide = true, requires_all = ["continuous", "queue"])]
    run_lease_lock: Option<std::os::fd::RawFd>,
}

#[derive(Subcommand)]
enum Command {
    /// Supervise the hub until stopped.
    ///
    /// A tunnel mode is required. There is no default.
    /// Set RADIO999K_TUNNEL=external and RADIO999K_PUBLIC_URL to record an
    /// externally managed HTTPS origin. This is the recommended option.
    /// Set RADIO999K_TUNNEL=tunnl to start the built-in SSH reverse tunnel for testing.
    /// Pass --env-file to read these settings from a private file instead.
    /// Use --local-only for loopback testing only. It ignores both sources.
    Run(cloud_service::RunOptions),
    #[command(hide = true)]
    CompanionGuard {
        lock: std::os::fd::RawFd,
        #[arg(last = true, required = true)]
        argv: Vec<std::ffi::OsString>,
    },
    /// Report supervised hub, queue, tunnel and delivery health.
    Health {
        #[arg(long)]
        home: Option<PathBuf>,
        /// Check configured public HTTPS routes without credentials.
        #[arg(long)]
        probe_public: bool,
    },
    /// Show the current lifecycle state of your hosted registration.
    HostedStatus {
        #[arg(long)]
        home: Option<PathBuf>,
    },
    /// Stage a local-only quickstart home and print next steps.
    Quickstart {
        #[arg(long, default_value = ".local-quickstart")]
        home: PathBuf,
    },
    /// Inspect one dead-lettered request in an owner-private queue without writing.
    DlqInspect {
        #[arg(long)]
        queue: PathBuf,
        #[arg(long)]
        id: String,
    },
    /// Deliver arrival identifiers to an owner-selected local command.
    NotifyArrivals {
        #[arg(long)]
        queue: PathBuf,
        #[arg(long)]
        registration: PathBuf,
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        command: PathBuf,
        #[arg(
            long = "mailbox",
            required_unless_present = "config",
            conflicts_with = "config"
        )]
        mailboxes: Vec<String>,
        /// Reload the owner-private managed recipient map each sweep.
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=3600))]
        interval_secs: Option<u64>,
    },
    /// Prepare Cloud settings, or stage a named mailbox in a dedicated local-only home.
    Init {
        agent: Option<String>,
        #[arg(long)]
        home: Option<PathBuf>,
        /// Prepare an operational mailbox policy and private bearer for Instinct or Cue.
        #[arg(long, requires = "agent")]
        operational: bool,
        /// Activate a staged replacement or apply a built-in profile.
        #[arg(long, requires = "agent", conflicts_with = "operational")]
        apply: bool,
        /// Use a matching existing private hub profile manifest.
        #[arg(long, requires = "agent", conflicts_with = "operational")]
        profiles_file: Option<PathBuf>,
        /// Select an email destination for Cue or Instinct.
        #[arg(long, requires = "agent", conflicts_with = "operational")]
        email_recipient: Option<String>,
        /// Select the owner-private executable that receives a destination and message ID.
        #[arg(long, requires = "email_recipient", conflicts_with = "operational")]
        relay_command: Option<PathBuf>,
        /// Reserve an address through the operator and approve in your browser.
        #[arg(long, conflicts_with_all = ["agent", "operational", "apply", "profiles_file", "email_recipient", "relay_command", "port"])]
        reserve_address: bool,
        /// Enable the private stdio socket during a managed apply; an already-active home can enable it once.
        #[arg(long, requires = "apply", conflicts_with = "reserve_address")]
        stdio_socket: bool,
        /// Discover authorized Orca peers on the local hub.
        #[arg(long, conflicts_with_all = ["agent", "operational", "apply", "profiles_file", "email_recipient", "relay_command", "port", "reserve_address"])]
        orca: bool,
        /// Authorized sender mailbox for Orca onboarding.
        #[arg(long, requires = "orca")]
        orca_sender: Option<String>,
        /// Private bearer file for the Orca sender.
        #[arg(long, requires = "orca")]
        orca_bearer_file: Option<PathBuf>,
        /// Set up one peer in Orca.
        #[arg(long, requires = "orca", value_parser = clap::builder::PossibleValuesParser::new(orca_onboard::PEERS.map(|(id, _)| id)))]
        orca_peer: Option<String>,
        /// Request owner-approved hosted setup for exactly this peer.
        #[arg(long, requires = "orca_peer", conflicts_with = "retry_orca_offer")]
        orca_reserve_address: bool,
        /// Private peer bearer file for the remote transport check.
        #[arg(long, requires = "orca_peer")]
        orca_remote_bearer_file: Option<PathBuf>,
        /// Check for a matching reply to this original request without sending mail.
        #[arg(long, requires = "orca_peer", conflicts_with_all = ["orca_reserve_address", "orca_email_recipient", "retry_orca_offer"])]
        orca_proof_original: Option<String>,
        /// Opt in to email arrival notifications for Cue or Instinct.
        #[arg(long, requires = "orca_peer", conflicts_with = "retry_orca_offer")]
        orca_email_recipient: Option<String>,
        /// Approved owner-private command for email arrival notifications.
        #[arg(long, requires = "orca_email_recipient")]
        orca_relay_command: Option<PathBuf>,
        /// Request a new card for an authorized peer after confirming the previous offer was lost.
        #[arg(long, requires = "orca", value_parser = clap::builder::PossibleValuesParser::new(orca_onboard::PEERS.map(|(id, _)| id)))]
        retry_orca_offer: Option<String>,
        /// Use a private Unix origin for a new hosted enrollment.
        #[arg(long, hide = true, requires = "reserve_address")]
        hosted_unix: bool,
        /// Set the managed hub's loopback port at first staging.
        #[arg(long, requires = "agent", conflicts_with = "operational", value_parser = clap::value_parser!(u16).range(1..))]
        port: Option<u16>,
    },
    /// Retire a dedicated mailbox, or preview removal of a managed profile.
    Remove {
        agent: String,
        #[arg(long)]
        home: PathBuf,
        /// Apply removal of a managed built-in profile.
        #[arg(long)]
        apply: bool,
    },
    #[command(hide = true)]
    ServeManaged {
        #[arg(long)]
        home: PathBuf,
    },
    #[command(hide = true)]
    ServeHostedConnector {
        #[arg(long)]
        home: PathBuf,
    },
    /// Manage a finite Bearer for Cue on the managed hosted Unix origin.
    HostedCueBearer {
        #[arg(long)]
        home: PathBuf,
        #[command(subcommand)]
        action: HostedCueBearerAction,
    },
    /// Prepare a private Muse queue, registration, policy, and Keychain bearer.
    SetupMuse {
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        peer: String,
    },
    /// Configure Grok Bot's inbound policy and central registration without Keychain.
    SetupGrok {
        /// Existing owner-private hub home. Edit shared registration only while the hub is stopped.
        #[arg(long)]
        home: PathBuf,
        /// SHA-256 hex digest of the owner-provided Bearer token, not the token itself.
        #[arg(long, value_name = "SHA256")]
        token_sha256: String,
    },
    /// Prepare the Codex Cloud mailbox, Keychain bearer, and optional Events storage.
    SetupCodex(codex_setup::SetupOptions),
    /// Store the Grok webhook pair in macOS Keychain.
    SetupGrokWake {
        #[arg(long, default_value = ".local")]
        home: PathBuf,
        /// Read the two existing mode 600 files instead of prompting. Require this HTTPS host.
        #[arg(long, value_name = "HOST")]
        from_private_files: Option<String>,
        /// Save the Keychain item without changing the wake selection.
        #[arg(long)]
        stage_only: bool,
    },
    /// Report app discovery and create any missing Cloud settings.
    Discover {
        #[arg(long, default_value = ".local")]
        home: PathBuf,
    },
    /// Report the missing authenticated workspace check. Does not prompt or connect.
    // NOTE: canonical Dot/OpenAI tunnel design; not wired yet, do not remove.
    #[command(hide = true)]
    DotTunnelSetup,
    /// Check the isolated Cloud Dot tunnel runtime.
    DotTunnelStatus {
        #[arg(long, default_value = ".local")]
        home: PathBuf,
    },
    /// Inspect local credential custody or explicitly import/export a protected handoff.
    Credentials(cloud_credentials::CredentialOptions),
    /// Offer or redeem a short-lived code for one peer mailbox.
    #[command(subcommand)]
    Pair(pairing_cli::Action),
    /// Copy a connector prompt, or separately copy its bearer from Keychain.
    Handoff {
        #[arg(long, default_value = ".local")]
        home: PathBuf,
        #[arg(long, value_enum)]
        app: cloud_credentials::App,
        #[arg(long)]
        copy_token: bool,
    },
    /// Prepare Dot storage in an existing private queue.
    DotPrepare {
        #[arg(long)]
        queue: PathBuf,
        #[arg(long)]
        auth_policy: PathBuf,
        /// Add Dot to an existing shared manifest without provisioning credentials.
        #[arg(long)]
        profiles_file: Option<PathBuf>,
    },
    GrokPrepare {
        #[arg(long)]
        queue: PathBuf,
        #[arg(long)]
        auth_policy: PathBuf,
    },
    /// Prepare Codex Cloud Events storage in an existing private queue.
    CodexPrepare {
        #[arg(long)]
        queue: PathBuf,
        #[arg(long)]
        auth_policy: PathBuf,
        #[arg(long)]
        registration: PathBuf,
    },
    MuseCheck {
        #[arg(long)]
        queue: PathBuf,
        #[arg(long)]
        auth_policy: PathBuf,
    },
    // FIXME(ponytail): overlaps setup-muse, which already provisions and prepares the same queue and policy.
    MusePrepare {
        #[arg(long)]
        queue: PathBuf,
        #[arg(long)]
        auth_policy: PathBuf,
    },
    ServeHub(Box<ServeHubOptions>),
    #[command(hide = true)]
    CodexPreviewStdio,
    /// Serve one configured local mailbox over MCP standard input and output.
    StdioMcp {
        #[arg(long)]
        home: PathBuf,
        #[arg(long)]
        mailbox: String,
        #[arg(long)]
        bearer_file: PathBuf,
        /// Restrict this bot connector to one recipient and its linked replies.
        #[arg(long)]
        peer: Option<String>,
    },
    // Disabled by owner request. Use the authenticated hub API/MCP for messages.
    // Keep these declarations with the disabled handlers below for a reversible change.
    /*
    Send {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, allow_hyphen_values = true)]
        body: String,
    },
    Reply {
        #[arg(long)]
        recipient: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        receipt: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long)]
        body: String,
    },
    */
    /// Local queue handoff for a bound Codex app bridge.
    CodexQueue {
        #[arg(long, default_value = "pigeonmq-cloud-queue.sqlite3")]
        db: PathBuf,
        #[arg(long)]
        id: String,
        #[arg(long)]
        sender: String,
        #[command(subcommand)]
        action: CodexQueueCommand,
    },
    // #FEEDBACK resolved: Use muse-check for readiness instead of a placeholder doctor.
}

#[cfg(target_os = "macos")]
fn disable_daemon_keychain_ui() -> security_framework::base::Result<()> {
    let guard = security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()?;
    // This is process-wide, not a per-read toggle. Keep UI disabled until exit,
    // including detached wake workers after a finite serve-hub run returns.
    std::mem::forget(guard);
    Ok(())
}

#[derive(clap::Subcommand)]
enum HostedCueBearerAction {
    /// Enable scoped Cue authorization and export a new Bearer to a private file.
    Rotate {
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=2592000))]
        lifetime_secs: u64,
        #[arg(long)]
        export_file: PathBuf,
    },
    /// Check the scoped policy state without showing a Bearer or digest.
    Status,
    /// Revoke the current scoped Bearer. Do not re-enable the old Bearer.
    Revoke,
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match &cli.command {
        Command::Pair(action) => pairing_cli::run(action).map_err(Into::into),
        Command::Credentials(options) => {
            cloud_credentials::run_credentials(options).map_err(Into::into)
        }
        Command::Handoff {
            home,
            app,
            copy_token,
        } => cloud_credentials::handoff(home, *app, *copy_token).map_err(Into::into),
        Command::DotTunnelSetup => cloud_setup::dot_tunnel_setup().map_err(Into::into),
        Command::DotTunnelStatus { home } => {
            cloud_setup::dot_tunnel_status(home).map_err(Into::into)
        }
        Command::Init {
            agent,
            home,
            operational,
            apply,
            profiles_file,
            email_recipient,
            relay_command,
            reserve_address,
            stdio_socket,
            hosted_unix,
            port,
            orca,
            orca_sender,
            orca_bearer_file,
            orca_peer,
            orca_reserve_address,
            orca_remote_bearer_file,
            orca_proof_original,
            orca_email_recipient,
            orca_relay_command,
            retry_orca_offer,
        } => {
            let home = cloud_setup::resolve_home(home.clone());
            if *reserve_address {
                return cloud_setup::reserve_address(&home, *hosted_unix);
            }
            if *orca {
                let explicit_peer = orca_peer.as_deref();
                let retry = retry_orca_offer.as_deref();
                if let (Some(peer), Some(retry_peer)) = (explicit_peer, retry)
                    && peer != retry_peer
                {
                    return Err(
                        "--orca-peer must match --retry-orca-offer when used together".into(),
                    );
                }
                // Check the requested provider before creating the local sender, peer, or enrollment.
                let provider = if *orca_reserve_address {
                    Some(hosted_enrollment::selected_provider(
                        explicit_peer.ok_or("select a hosted peer")?,
                    )?)
                } else {
                    None
                };
                let sender = orca_sender.as_deref().unwrap_or("codex-cloud");
                let default_managed_sender = cfg!(target_os = "macos")
                    && orca_sender.is_none()
                    && orca_bearer_file.is_none();
                let managed_nonretry =
                    default_managed_sender && retry.is_none() && orca_proof_original.is_none();
                let bearer_file = match orca_bearer_file {
                    Some(path) => path.clone(),
                    None if managed_nonretry => lifecycle::orca_sender_file(&home)?,
                    None => orca_onboard::resolve_bearer_file(&home, sender)?,
                };
                return orca_onboard::onboard(
                    &home,
                    sender,
                    &bearer_file,
                    orca_onboard::OnboardOptions {
                        selected: explicit_peer.or(retry),
                        retry: retry.is_some(),
                        may_provision: managed_nonretry,
                        remote: orca_onboard::RemoteOptions {
                            provider,
                            bearer_file: orca_remote_bearer_file.as_deref(),
                        },
                        proof_original: orca_proof_original.as_deref(),
                        email_recipient: orca_email_recipient.as_deref(),
                        relay_command: orca_relay_command.as_deref(),
                    },
                );
            }
            if *operational {
                let mailbox = agent.as_deref().ok_or("mailbox name required")?;
                lifecycle::check_manual_stage(&home, mailbox)?;
                println!("{}", reply_mailbox_setup::stage(&home, mailbox)?);
                return Ok(());
            }
            if let Some(name) = agent {
                if *stdio_socket && !lifecycle::supported(name) {
                    return Err("managed stdio requires a supported managed profile".into());
                }
                let result = if lifecycle::supported(name) {
                    lifecycle::init(
                        &home,
                        name,
                        &lifecycle::Options {
                            profiles_file: profiles_file.as_deref(),
                            email_recipient: email_recipient.as_deref(),
                            relay_command: relay_command.as_deref(),
                            port: *port,
                            local_stdio: *stdio_socket,
                            apply: *apply,
                        },
                    )?
                } else {
                    if profiles_file.is_some()
                        || email_recipient.is_some()
                        || relay_command.is_some()
                        || port.is_some()
                    {
                        return Err("built-in lifecycle options require a supported profile".into());
                    }
                    runtime_profiles::init(&home, name, *apply)?
                };
                println!("{result}");
                Ok(())
            } else {
                cloud_setup::setup(&home, true)
            }
        }
        Command::Remove { agent, home, apply } => {
            let result = if lifecycle::supported(agent) {
                lifecycle::remove(home, agent, *apply)?
            } else {
                runtime_profiles::remove(home, agent)?
            };
            println!("{result}");
            Ok(())
        }
        Command::ServeManaged { home } => {
            #[cfg(target_os = "macos")]
            disable_daemon_keychain_ui()?;
            lifecycle::serve(home).map_err(Into::into)
        }
        Command::ServeHostedConnector { home } => {
            lifecycle::serve_hosted_connector(home).map_err(Into::into)
        }
        Command::HostedCueBearer { home, action } => {
            match action {
                HostedCueBearerAction::Rotate {
                    lifetime_secs,
                    export_file,
                } => {
                    lifecycle::hosted_cue_bearer(home, Some((*lifetime_secs, export_file)))?;
                }
                HostedCueBearerAction::Revoke => lifecycle::hosted_cue_bearer(home, None)?,
                HostedCueBearerAction::Status => {
                    println!("{}", lifecycle::hosted_cue_bearer_status(home)?);
                    return Ok(());
                }
            }
            println!("Hosted Cue Bearer state updated.");
            Ok(())
        }
        Command::SetupMuse { home, peer } => cloud_setup::setup_muse(home, peer),
        Command::SetupGrok { home, token_sha256 } => {
            cloud_setup::setup_grok(home, token_sha256).map_err(Into::into)
        }
        Command::SetupCodex(options) => codex_setup::setup(options).map_err(Into::into),
        Command::SetupGrokWake {
            home,
            from_private_files,
            stage_only,
        } => cloud_credentials::setup_grok_wake(home, from_private_files.as_deref(), *stage_only)
            .map_err(Into::into),
        Command::Discover { home } => cloud_setup::setup(home, false),
        Command::Run(options) => cloud_service::start(options).map_err(Into::into),
        Command::CompanionGuard { lock, argv } => {
            cloud_service::companion_guard(*lock, argv).map_err(Into::into)
        }
        Command::Health { home, probe_public } => std::process::exit(cloud_service::health(
            &cloud_setup::resolve_home(home.clone()),
            *probe_public,
        )),
        Command::HostedStatus { home } => {
            hosted_enrollment::print_status(&cloud_setup::resolve_home(home.clone()))
                .map_err(Into::into)
        }
        Command::Quickstart { home } => cloud_setup::quickstart(home),
        Command::NotifyArrivals {
            queue,
            registration,
            state,
            command,
            mailboxes,
            config,
            interval_secs,
        } => {
            if config.is_none() {
                radio999k::arrival_notify::validate_static_mailboxes(mailboxes)?;
            }
            let sweep = || match config {
                Some(path) => {
                    radio999k::arrival_notify::run_config(queue, registration, state, command, path)
                }
                None => {
                    radio999k::arrival_notify::run(queue, registration, state, command, mailboxes)
                }
            };
            let Some(interval) = interval_secs else {
                println!("{}", sweep()?);
                return Ok(());
            };
            loop {
                match sweep() {
                    Ok(status)
                        if status["delivered"] != 0
                            || status["expired"].as_u64().unwrap_or(0) > 0 =>
                    {
                        println!("{status}")
                    }
                    Ok(_) => {}
                    Err(error) if config.is_some() => {
                        eprintln!(
                            "{}",
                            radio999k::arrival_notify::safe_failure(error.as_ref())
                        );
                    }
                    Err(_) => eprintln!("arrival notification sweep incomplete"),
                }
                std::thread::sleep(Duration::from_secs(*interval));
            }
        }
        Command::DlqInspect { queue, id } => {
            let result = Queue::inspect_dead_letter(queue, id).map_err(|error| match error {
                radio999k::Error::Database(_) => {
                    radio999k::Error::Invalid("queue_read_unavailable")
                }
                other => other,
            })?;
            println!("{}", serde_json::to_string(&result)?);
            Ok(())
        }
        Command::CodexPrepare {
            queue,
            auth_policy,
            registration,
        } => {
            let profile =
                mailbox_startup::explicit(catalog::codex(), queue, auth_policy, registration);
            profile.validate().map_err(|(_, reason)| reason)?;
            let events = codex_events::Runtime::new(&profile)
                .map_err(|(_, reason)| reason)?
                .ok_or("Codex Events disabled")?;
            events.prepare().map_err(|error| error.reason)?;
            println!(
                "{}",
                serde_json::json!({"state":"codex_events_storage_prepared","runtime_verified":false})
            );
            Ok(())
        }
        Command::MuseCheck { queue, auth_policy } => {
            let ready = local_http::check_muse(queue, auth_policy)?;
            println!(
                "{}",
                serde_json::json!({"state": if ready {"ready_local_only"} else {"muse_storage_unprepared"}, "agent":"muse","runtime_verified":false})
            );
            if !ready {
                return Err("Muse storage unprepared".into());
            }
            Ok(())
        }
        Command::MusePrepare { queue, auth_policy } => {
            cloud_credentials::provision(auth_policy, cloud_credentials::App::Muse)?;
            local_http::prepare_muse(queue, auth_policy)?;
            println!(
                "{}",
                serde_json::json!({"state":"muse_storage_prepared","runtime_verified":false})
            );
            Ok(())
        }
        Command::DotPrepare {
            queue,
            auth_policy,
            profiles_file,
        } => {
            if let Some(manifest) = profiles_file {
                let added = mailbox_startup::prepare_dot_binding(queue, auth_policy, manifest)?;
                println!(
                    "{}",
                    serde_json::json!({
                        "state":"dot_storage_prepared","binding_added":added,"runtime_verified":false
                    })
                );
                return Ok(());
            }
            cloud_credentials::provision(auth_policy, cloud_credentials::App::Dot)?;
            radio999k::dot_profile::check(queue, auth_policy, true).map_err(Into::into)
        }
        Command::GrokPrepare { queue, auth_policy } => {
            cloud_credentials::provision(auth_policy, cloud_credentials::App::GrokBot)?;
            local_http::prepare_grok(queue, auth_policy).map_err(Into::into)
        }
        Command::CodexPreviewStdio => lifecycle::codex_preview_stdio().map_err(Into::into),
        Command::StdioMcp {
            home,
            mailbox,
            bearer_file,
            peer,
        } => stdio_mcp::run(home, mailbox, bearer_file, peer.as_deref()).map_err(Into::into),
        Command::ServeHub(options) => {
            let ServeHubOptions {
                queue,
                mailbox_home,
                profiles_file,
                profiles_fingerprint,
                auth_policy,
                bundled_profiles,
                port,
                seconds,
                continuous,
                stdio_socket,
                run_lease_lock,
            } = options.as_ref();
            if let Some(lock) = run_lease_lock {
                cloud_service::keep_hub_lease(*lock)?;
            }
            // Only the unattended hub command suppresses Keychain prompts.
            // Interactive setup/credentials commands run in separate processes.
            #[cfg(target_os = "macos")]
            disable_daemon_keychain_ui()?;
            let seconds = if *continuous { None } else { *seconds };
            let local = mailbox_home
                .as_deref()
                .map(runtime_profiles::load)
                .transpose()?;
            let (queue, local_profiles) = match local {
                Some(local) => (local.queue, local.profiles),
                None => (
                    queue.clone().ok_or("explicit --queue required")?,
                    Vec::new(),
                ),
            };
            let home = queue.parent().ok_or("queue parent required")?;
            let manifest = profiles_file
                .as_deref()
                .map(|path| mailbox_startup::Manifest::load(path, home))
                .transpose()?;
            if let Some(expected) = profiles_fingerprint
                && !manifest
                    .as_ref()
                    .is_some_and(|file| file.fingerprint() == expected)
            {
                return Err("Hub profile manifest changed before hub startup".into());
            }
            let bindings = if *bundled_profiles || manifest.is_some() {
                mailbox_startup::bindings(home, manifest.as_ref())
            } else {
                Vec::new()
            };
            local_http::serve_hub(
                queue,
                auth_policy.clone(),
                bindings,
                local_profiles,
                manifest
                    .as_ref()
                    .map(mailbox_startup::Manifest::fingerprint),
                *port,
                seconds,
                *stdio_socket,
            )
            .map_err(Into::into)
        }
        Command::CodexQueue {
            db,
            id,
            sender,
            action,
        } => {
            let mut queue = Queue::open(std::fs::canonicalize(db)?)?;
            let result = match action {
                CodexQueueCommand::Claim => serde_json::to_value(queue.receive_first_exact(
                    CODEX_MAILBOX,
                    id,
                    sender,
                    Duration::from_secs(600),
                )?)?,
                CodexQueueCommand::Complete => {
                    let input: CodexFinal = serde_json::from_reader(std::io::stdin().lock())?;
                    let message = queue.observe_ordinary(id, CODEX_MAILBOX)?;
                    if message.envelope.sender != *sender || message.envelope.in_reply_to.is_some()
                    {
                        return Err("Codex queue binding mismatch".into());
                    }
                    serde_json::to_value(
                        queue.reply_and_ack(
                            CODEX_MAILBOX,
                            id,
                            &input.receipt,
                            radio999k::Envelope {
                                sender: CODEX_MAILBOX.into(),
                                recipient: sender.clone(),
                                body: input.body,
                                correlation_id: Some(
                                    message
                                        .envelope
                                        .correlation_id
                                        .unwrap_or_else(|| id.clone()),
                                ),
                                in_reply_to: Some(id.clone()),
                                idempotency_key: None,
                            },
                        )?,
                    )?
                }
            };
            println!("{result}");
            Ok(())
        }
    }
    /* Direct-DB message handlers disabled with their command declarations above.
    Restore `_ => {}` in the match above only with these commands.
    if let Command::Send { from, to, body } = &cli.command {
        let mut queue = Queue::open(cli.db.clone())?;
        let envelope = radio999k::Envelope {
            sender: from.clone(),
            recipient: to.clone(),
            body: body.clone(),
            correlation_id: None,
            in_reply_to: None,
            idempotency_key: None,
        };
        let sent = queue.send(envelope)?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::to_value(sent)?)?
        );
        return Ok(());
    }
    if let Command::Reply {
        recipient,
        id,
        receipt,
        from,
        to,
        body,
    } = &cli.command
    {
        let mut queue = Queue::open(cli.db.clone())?;
        let reply = queue.reply_and_ack(
            recipient,
            id,
            receipt,
            radio999k::Envelope {
                sender: from.clone(),
                recipient: to.clone(),
                body: body.clone(),
                correlation_id: None,
                in_reply_to: Some(id.clone()),
                idempotency_key: None,
            },
        )?;
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::to_value(reply)?)?
        );
        return Ok(());
    }
    Ok(())
    */
}

fn main() {
    // #FEEDBACK resolved: Restore the entry point diagnostics to stderr.
    radio999k::logging::init();
    if let Err(error) = run() {
        eprintln!("{}", error);
        std::process::exit(1);
    }
}

#[cfg(all(test, target_os = "macos"))]
#[test]
fn daemon_keychain_ui_is_process_local() {
    use security_framework::os::macos::keychain::SecKeychain;
    const CHILD: &str = "PIGEONMQ_TEST_DAEMON_KEYCHAIN_UI";
    if std::env::var_os(CHILD).is_some() {
        disable_daemon_keychain_ui().unwrap();
        assert!(!SecKeychain::user_interaction_allowed().unwrap());
        // The setting must still apply after the helper returns and on workers.
        std::thread::spawn(|| {
            assert!(!SecKeychain::user_interaction_allowed().unwrap());
        })
        .join()
        .unwrap();
        return;
    }
    let original = SecKeychain::user_interaction_allowed().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "daemon_keychain_ui_is_process_local"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    assert!(child.status.success(), "isolated Keychain UI check failed");
    assert_eq!(SecKeychain::user_interaction_allowed().unwrap(), original);
}

#[cfg(test)]
mod orca_cli_tests {
    use super::*;

    #[test]
    fn orca_peer_flags_only_accept_supported_ids_when_orca_is_enabled() {
        for (peer, _) in orca_onboard::PEERS {
            assert!(Cli::try_parse_from(["999k", "init", "--orca", "--orca-peer", peer]).is_ok());
            assert!(
                Cli::try_parse_from(["999k", "init", "--orca", "--retry-orca-offer", peer]).is_ok()
            );
        }
        for flag in ["--orca-peer", "--retry-orca-offer"] {
            assert!(Cli::try_parse_from(["999k", "init", "--orca", flag, "unknown"]).is_err());
            assert!(Cli::try_parse_from(["999k", "init", flag, "dot"]).is_err());
        }
        assert!(Cli::try_parse_from(["999k", "init", "--orca"]).is_ok());
    }
}
