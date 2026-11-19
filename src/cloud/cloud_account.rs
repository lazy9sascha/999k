//! Read the current ChatGPT plan through the Codex CLI without refreshing login.
// NOTE: canonical Dot/OpenAI tunnel design; not wired yet, do not remove.
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

pub(crate) fn chatgpt_plan() -> Option<String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(5), read_plan())
                .await
                .ok()
                .flatten()
        })
}

async fn read_plan() -> Option<String> {
    let mut child = Command::new("codex")
        .args(["app-server", "--listen", "stdio://"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut input = child.stdin.take()?;
    let mut lines = BufReader::new(child.stdout.take()?).lines();

    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"clientInfo": {"name": env!("CARGO_PKG_NAME"), "version": env!("CARGO_PKG_VERSION")}}
    });
    let mut line = serde_json::to_vec(&initialize).ok()?;
    line.push(b'\n');
    input.write_all(&line).await.ok()?;
    response(&mut lines, 1).await?;

    input
        .write_all(
            br#"{"jsonrpc":"2.0","method":"initialized"}
{"jsonrpc":"2.0","id":2,"method":"account/read","params":{"refreshToken":false}}
"#,
        )
        .await
        .ok()?;
    let result = response(&mut lines, 2).await?;
    let account = &result["account"];
    if account["type"] != "chatgpt" {
        return None;
    }
    account["planType"]
        .as_str()
        .filter(|plan| {
            !plan.is_empty()
                && plan.len() <= 40
                && plan.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .map(str::to_owned)
}

async fn response(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Option<Value> {
    while let Some(line) = lines.next_line().await.ok()? {
        let Ok(mut message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message["id"].as_u64() == Some(id) {
            return message.get_mut("result").map(Value::take);
        }
    }
    None
}
