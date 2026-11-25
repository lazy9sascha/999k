//! Public setup text for the owner to paste in the selected peer's own chat.
const MUSE_POLLING: &str = "On the Muse VM, enable one persistent mailbox poll every 2 minutes. Reuse the existing schedule on repeat setup. Store the poller in your home directory and load the credential privately at runtime. Stay quiet when the inbox is empty. Hand each new request to Muse once by its original message ID and use p-999k_reply for a linked answer under your normal approval rules. Report the active schedule and next run. Verify automatic intake with a new request while this chat is idle.";

pub(crate) fn intake(peer: &str) -> &'static str {
    match peer {
        "dot" => {
            "In this Dot conversation, enable the private P-999K custom MCP connector through Secure MCP Tunnel. Use tunnel details only in the connector settings. List the mailbox tools from this conversation."
        }
        "muse" => MUSE_POLLING,
        "grok-bot" => {
            "In the existing GrokBot app, use its private runtime to read the original and use p-999k_reply with its original message ID. Reuse or configure one dedicated wake routine. If you use a VM-local poller to trigger it, report its interval and whether it survives a VM restart."
        }
        "cue" => {
            "Use arrival email as a notification. Open the original through authenticated MCP and use p-999k_reply with its original message ID. If your hosted app has no connector, use its HTTPS-capable runtime to make the MCP calls. Keep message content and replies in P-999K."
        }
        "instinct" => {
            "Use the existing Instinct iMessage conversation for setup. If the app offers a secure login form, enter the code there. Redeem with a plain HTTPS JSON POST and save the Bearer in private storage. Use authenticated MCP JSON-RPC requests from the app's runtime to read the original and call p-999k_reply with its original message ID. No connector installation is needed."
        }
        _ => "",
    }
}

pub(crate) const VERIFY: &str = "Report mailbox discovery and arrival handling separately. For a live check, read a new request and post a linked reply using its original message ID. If you enabled automatic intake, verify it with a new request while this chat is idle.";

pub(crate) fn instructions(peer: &str) -> String {
    if peer == "dot" {
        return format!(
            "Paste in Dot's own chat:\n\n{} {VERIFY} Keep credentials out of chat.",
            intake(peer)
        );
    }
    let app = match peer {
        "muse" => "Muse",
        "grok-bot" => "GrokBot",
        "cue" => "Cue",
        "instinct" => "Instinct",
        _ => return "No setup instruction is available for this peer.".into(),
    };
    let intake = intake(peer);
    let route = &radio999k::mailbox_catalog::http_by_id(peer)
        .expect("built-in Orca peer has an HTTP route")
        .route;
    if peer != "muse" {
        let (_, pair) = crate::pairing::route(peer).expect("remote Orca peer supports pairing");
        return format!(
            "Paste in {app}'s own private chat:\n\nConnect your existing {app} app to the P-999K mailbox `{peer}` at `{route}`. Use the owner-approved HTTPS origin and redeem the one-time code via JSON POST to `{pair}`. Keep the code and returned Bearer private. Authenticate MCP initialize and tools/list with the saved credential. {intake} {VERIFY}\n\nOn the hub Mac, run `999k pair offer {peer}` in your own terminal to generate the code and ready-to-paste offer. The code expires in 10 minutes."
        );
    }
    let handoff = format!(
        "Paste in {app}'s own chat:\n\nConnect your existing {app} agent to my P-999K mailbox `{peer}` at the MCP route `{route}`. Use the reachable hub address and the mailbox credential from your private connector settings. If either is missing, tell me which setting you need. Keep credentials out of chat. {intake} List the available mailbox tools. {VERIFY}"
    );
    if crate::pairing::route(peer).is_none() {
        return handoff;
    }
    let bearer_env = format!(
        "P999K_{}_BEARER",
        peer.replace('-', "_").to_ascii_uppercase()
    );
    format!(
        r#"{handoff}

Pairing is an optional owner action. It uses the exact HTTPS hub origin that serves this mailbox. Once that origin is reachable, run this in your own terminal to create one code:
  999k pair offer {peer}

Send the offer text to the selected peer's private chat. Its single-use code expires in 10 minutes. The receiving client stores the resulting credential privately.

On the receiving computer, install the 999k release binary for its OS and architecture. Open a private Bash terminal. Set HUB_ORIGIN to the same owner-confirmed HTTPS origin and turn off shell tracing. Then run:
  (umask 077 && mkdir -p "$HOME/.p-999k-{peer}")
  IFS= read -rs -p 'Pairing code: ' code
  printf '%s\n' "$code" | 999k pair redeem {peer} --origin "$HUB_ORIGIN" --code-stdin --output "$HOME/.p-999k-{peer}/bearer"
  unset code

Stop if the command does not confirm that the bearer was saved. Set the remote MCP URL to "$HUB_ORIGIN{route}". Store the saved bearer in the existing connector's private credential settings. If the hosted client has no secure import, ask the owner to configure its private connector. A separate terminal client can verify the mailbox route. The hosted client still needs its own connector.

For a terminal Codex client, close its old session. Configure it and start a fresh one from the same private Bash terminal:
  codex mcp add p-999k-{peer} --url "$HUB_ORIGIN{route}" --bearer-token-env-var {bearer_env}
  {bearer_env}="$(cat "$HOME/.p-999k-{peer}/bearer")" codex

For other clients, restart the client after changing its connector. List the mailbox tools, then verify a linked reply separately."#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_uses_each_existing_apps_intake_instructions() {
        for (peer, app, intake_detail) in [
            ("grok-bot", "GrokBot", "dedicated wake routine"),
            ("cue", "Cue", "arrival email as a notification"),
            (
                "instinct",
                "Instinct",
                "existing Instinct iMessage conversation",
            ),
        ] {
            let text = instructions(peer);
            let (mcp, pair) = crate::pairing::route(peer).unwrap();
            assert!(text.contains(app) && text.contains(mcp) && text.contains(pair));
            assert!(text.contains(intake(peer)) && text.contains(intake_detail));
            assert!(text.contains(VERIFY));
            assert!(!text.contains("install the 999k release binary"));
        }
        let muse = instructions("muse");
        assert!(muse.contains(MUSE_POLLING) && muse.contains("install the 999k release binary"));
        let dot = instructions("dot");
        assert!(dot.contains(intake("dot")) && dot.contains("Secure MCP Tunnel"));
        assert!(dot.contains(VERIFY) && !dot.contains("pair offer"));
        assert!(instructions("instinct").contains("secure login form"));
        assert!(instructions("cue").contains("HTTPS-capable runtime"));
    }
}
