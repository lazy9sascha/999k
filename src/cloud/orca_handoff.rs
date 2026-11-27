//! Public setup text for the owner to paste in the selected peer's own chat.
const MUSE_POLLING: &str = "On the Muse VM, enable one persistent mailbox poll every 2 minutes. Reuse the existing schedule on repeat setup. Store the poller in your home directory and load the credential privately at runtime. Stay quiet when the inbox is empty. Hand each new request to Muse once by its original message ID and use p-999k_reply for a linked answer under your normal approval rules. Report the active schedule and next run. Verify automatic intake with a new request while this chat is idle.";

pub(crate) fn intake(peer: &str) -> &'static str {
    match peer {
        "dot" => {
            "For direct OAuth, follow the setup terminal instructions after it reports OAuth discovery ready. If you already use a private Secure MCP Tunnel, keep that connector. Use connection details only in connector settings. In the intended Dot conversation, list the mailbox tools."
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
    if peer == "cue" {
        return "After the setup terminal reports authenticated MCP access, open P-999K → Cue → I’ll do it myself. Copy the setup message into your own existing Cue chat. For the same message with the RC3 plugin or CLI alone, see ‘Connect your existing Cue chat’ at https://github.com/lazy9sascha/p-999k/releases/tag/0.2.1-rc.3. For this Mac’s connection, wait for its private form before running `999k pair offer cue` in your local terminal. Enter the displayed address and code privately. Send a question from your Orca project chat after Cue confirms tool discovery at this Mac’s address.".into();
    }
    let app = match peer {
        "muse" => "Muse",
        "grok-bot" => "GrokBot",
        "instinct" => "Instinct",
        _ => return "No setup instruction is available for this peer.".into(),
    };
    let (mcp, pair) = crate::pairing::route(peer).expect("remote Orca peer supports pairing");
    let intake = intake(peer);
    format!(
        "On the hub Mac: if the peer needs enrollment, configure its HTTPS routes and run `999k pair offer {peer}` in your private terminal. Use the peer's secure form for the code when available.\n\nMessage for {app}:\nConnect your existing {app} runtime to the P-999K mailbox `{peer}` at `{mcp}`. If you already have a saved credential, reuse it. Otherwise redeem the owner's single-use code over HTTPS at `{pair}` and save the returned credential privately. Initialize MCP and list its tools. {intake} {VERIFY}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_uses_each_existing_apps_intake_instructions() {
        for (peer, app, intake_detail) in [
            ("muse", "Muse", MUSE_POLLING),
            ("grok-bot", "GrokBot", "dedicated wake routine"),
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
            let (owner, message) = text.split_once(&format!("Message for {app}:")).unwrap();
            assert!(owner.contains(&format!("999k pair offer {peer}")));
            assert!(!message.contains("pair offer") && !message.contains("codex mcp"));
        }
        let dot = instructions("dot");
        assert!(dot.contains(intake("dot")) && dot.contains("Secure MCP Tunnel"));
        assert!(dot.contains(VERIFY) && !dot.contains("pair offer"));
        assert!(instructions("instinct").contains("secure login form"));
        let cue = instructions("cue");
        assert!(
            cue.contains("your own existing Cue chat")
                && cue.contains("Connect your existing Cue chat")
        );
        assert!(cue.contains("wait for its private form before running `999k pair offer cue`"));
        assert!(cue.contains("after Cue confirms tool discovery"));
    }
}
