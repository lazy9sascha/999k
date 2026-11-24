//! Read-only checks for the protected existing Codex owner. Never resumes or submits except recovery resume of the same thread_id.
use crate::{
    adapters::codex::native_transport::NativeTransport, adapters::codex::receiver::Binding,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::{Duration, Instant};
pub fn thread_matches(binding: &Binding, value: &Value) -> bool {
    value["id"] == binding.thread_id
        && value["cwd"].as_str() == binding.cwd.to_str()
        && value["ephemeral"] != true
        && matches!(value["status"]["type"].as_str(), Some("idle" | "active"))
}
pub async fn preflight(native: &mut NativeTransport, binding: &Binding) -> Result<(), String> {
    // Recovery (CODEXSA-034): if the approved thread is not loaded (e.g. after
    // a restart or socket loss), try to restore the SAME thread_id via
    // thread/resume. This is recovery, not admission: it never substitutes
    // another thread. A failed resume leaves work uncertain.
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    let mut loaded = false;
    for _ in 0..100 {
        let mut params = json!({"limit":100});
        if let Some(c) = cursor.take() {
            params["cursor"] = c
        }
        let page = native
            .request(
                "thread/loaded/list",
                params,
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .map_err(|e| e.to_string())?;
        let ids = page["data"].as_array().ok_or("invalid_loaded_list")?;
        if ids.iter().any(|id| id == &binding.thread_id) {
            loaded = true;
            break;
        }
        if page["nextCursor"].is_null() {
            break;
        }
        let c = page["nextCursor"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or("invalid_loaded_cursor")?;
        if !seen.insert(c.to_owned()) {
            return Err("invalid_loaded_cursor".into());
        }
        cursor = Some(json!(c));
    }
    if !loaded {
        // Attempt recovery resume for the SAME thread_id only.
        let resume = native
            .request(
                "thread/resume",
                json!({"threadId": binding.thread_id}),
                Instant::now() + Duration::from_secs(10),
            )
            .await;
        if resume.is_err() {
            return Err("codex_thread_resume_failed".into());
        }
        // Re-check loaded set after resume.
        let mut cursor2 = None;
        let mut seen2 = std::collections::HashSet::new();
        for _ in 0..100 {
            let mut params = json!({"limit":100});
            if let Some(c) = cursor2.take() {
                params["cursor"] = c
            }
            let page = native
                .request(
                    "thread/loaded/list",
                    params,
                    Instant::now() + Duration::from_secs(10),
                )
                .await
                .map_err(|e| e.to_string())?;
            let ids = page["data"].as_array().ok_or("invalid_loaded_list")?;
            if ids.iter().any(|id| id == &binding.thread_id) {
                loaded = true;
                break;
            }
            if page["nextCursor"].is_null() {
                break;
            }
            let c = page["nextCursor"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or("invalid_loaded_cursor")?;
            if !seen2.insert(c.to_owned()) {
                return Err("invalid_loaded_cursor".into());
            }
            cursor2 = Some(json!(c));
        }
        if !loaded {
            return Err("codex_bound_thread_not_loaded".into());
        }
    }
    let r = native
        .request(
            "thread/read",
            json!({"threadId":binding.thread_id,"includeTurns":false}),
            Instant::now() + Duration::from_secs(10),
        )
        .await
        .map_err(|e| e.to_string())?;
    if !thread_matches(binding, &r["thread"]) {
        return Err("codex_native_binding_mismatch".into());
    }
    Ok(())
}
pub async fn connect_native(binding: &Binding) -> Result<NativeTransport, String> {
    let mut native =
        NativeTransport::connect(&binding.endpoint, Instant::now() + Duration::from_secs(10))
            .await
            .map_err(|e| e.to_string())?;
    native.initialize(json!({"clientInfo":{"name":"999k-codex-receiver","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}),Instant::now()+Duration::from_secs(10)).await.map_err(|e|e.to_string())?;
    Ok(native)
}

/// Unattended turns on the bound thread (app-server 0.169 `thread/settings/update`):
/// approvalPolicy "never" so no human is asked, and a workspace-write sandbox
/// limited to the bound cwd so repository work runs without escalation.
pub async fn set_unattended(native: &mut NativeTransport, binding: &Binding) -> Result<(), String> {
    native
        .request(
            "thread/settings/update",
            json!({"threadId":binding.thread_id,"approvalPolicy":"never",
                "sandboxPolicy":{"type":"workspaceWrite","writableRoots":[binding.cwd],"networkAccess":false}}),
            Instant::now() + Duration::from_secs(15),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}
/// Explicit setup only. The caller must hold its receiver guard with no pending work.
pub async fn attach_saved(native: &mut NativeTransport, binding: &Binding) -> Result<(), String> {
    let response = native
        .request(
            "thread/resume",
            json!({"threadId":binding.thread_id}),
            Instant::now() + Duration::from_secs(15),
        )
        .await
        .map_err(|e| e.to_string())?;
    if !thread_matches(binding, &response["thread"]) {
        return Err("codex_native_binding_mismatch".into());
    }
    Ok(())
}

/// Durable position in the native queue/history scan. Not an admission or completion proof.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionScan {
    #[serde(default)]
    turns: bool,
    cursor: Option<String>,
}

/// Read-only reconciliation in bounded pages. The caller persists `scan` even
/// after an error, so later passes continue beyond the same newest history.
pub async fn find_submission(
    native: &mut NativeTransport,
    binding: &Binding,
    client: &str,
    scan: &mut SubmissionScan,
    mut authorize: impl FnMut() -> Result<(), String>,
) -> Result<crate::adapters::codex::receiver::Found, String> {
    use crate::adapters::codex::receiver::Found;
    let mut seen = std::collections::HashSet::new();
    if let Some(cursor) = &scan.cursor {
        seen.insert((scan.turns, cursor.clone()));
    }
    for _ in 0..40 {
        authorize()?;
        let limit = if scan.turns { 50 } else { 100 };
        let mut params = json!({"threadId":binding.thread_id,"limit":limit});
        if scan.turns {
            params["itemsView"] = json!("full");
            params["sortDirection"] = json!("desc");
        }
        if let Some(cursor) = &scan.cursor {
            params["cursor"] = json!(cursor);
        }
        let method = if scan.turns {
            "thread/turns/list"
        } else {
            "thread/queue/list"
        };
        let page = native
            .request(method, params, Instant::now() + Duration::from_secs(15))
            .await
            .map_err(|e| e.to_string())?;
        let data = page["data"].as_array().ok_or("invalid_submission_list")?;
        if data.len() > limit {
            return Err("invalid_submission_list".into());
        }
        let found = if scan.turns {
            data.iter().find_map(|turn| {
                let items = turn["items"].as_array().map(Vec::as_slice).unwrap_or(&[]);
                if !items
                    .iter()
                    .any(|item| item["type"] == "userMessage" && item["clientId"] == client)
                {
                    return None;
                }
                Some(match turn["status"].as_str() {
                    Some("completed") => items
                        .iter()
                        .rev()
                        .find(|item| {
                            item["type"] == "agentMessage" && item["phase"] == "final_answer"
                        })
                        .and_then(|item| item["text"].as_str())
                        .filter(|text| !text.trim().is_empty())
                        .map_or(Found::Unknown, |text| Found::Final(text.to_owned())),
                    Some(status @ ("failed" | "interrupted")) => {
                        Found::Failed(format!("native_turn_{status}"))
                    }
                    _ => Found::Unknown,
                })
            })
        } else {
            data.iter()
                .any(|item| item["clientUserMessageId"] == client)
                .then_some(Found::Unknown)
        };
        if let Some(found) = found {
            authorize()?;
            *scan = SubmissionScan::default();
            return Ok(found);
        }
        let next = match page.get("nextCursor") {
            Some(Value::Null) => None,
            Some(Value::String(cursor)) if !cursor.is_empty() => Some(cursor.clone()),
            _ => return Err("invalid_submission_cursor".into()),
        };
        if let Some(cursor) = &next
            && !seen.insert((scan.turns, cursor.clone()))
        {
            return Err("invalid_submission_cursor".into());
        }
        scan.cursor = next;
        if scan.cursor.is_none() {
            if scan.turns {
                authorize()?;
                *scan = SubmissionScan::default();
                return Ok(Found::NotThere);
            }
            scan.turns = true;
        }
    }
    Ok(Found::Unknown)
}
