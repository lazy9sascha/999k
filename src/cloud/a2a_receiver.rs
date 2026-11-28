use radio999k::{
    Delivery, mcp_request,
    private_file::{MAX_PRIVATE_CONFIG_BYTES, open_private_file},
    receiver_api::{Action, Request},
    receiver_client::{Client as HubClient, Transport},
    runtime_admission::Record,
};
use reqwest::{Url, blocking::Client, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    net::IpAddr,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

pub(crate) const MAILBOX: &str = "a2a";
const LEASE_SECONDS: u64 = 30;
const POLL_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_REPLY_BYTES: usize = 16_384;

type Result<T> = std::result::Result<T, &'static str>;

#[derive(clap::Args)]
pub(crate) struct Options {
    /// Owner-private JSON with the A2A endpoint and credential file paths.
    #[arg(long)]
    binding: PathBuf,
    /// Process at most one queued message, then exit.
    #[arg(long)]
    once: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    endpoint: String,
    hub_port: u16,
    hub_token_file: PathBuf,
    agent_token_file: Option<PathBuf>,
}

impl Binding {
    fn read(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        open_private_file(path)
            .map_err(|_| "a2a_binding_unavailable")?
            .take(MAX_PRIVATE_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "a2a_binding_unavailable")?;
        if bytes.len() as u64 > MAX_PRIVATE_CONFIG_BYTES {
            return Err("invalid_a2a_binding");
        }
        serde_json::from_value(mcp_request::strict_json(&bytes).map_err(|_| "invalid_a2a_binding")?)
            .map_err(|_| "invalid_a2a_binding")
    }
}

struct Agent {
    client: Client,
    endpoint: Url,
    token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Submission {
    Message(AgentMessage),
    Task(Task),
}

#[derive(Deserialize)]
struct AgentMessage {
    role: String,
    parts: Vec<Part>,
}

#[derive(Deserialize)]
struct Part {
    text: Option<String>,
}

#[derive(Deserialize)]
struct Artifact {
    parts: Vec<Part>,
}

#[derive(Deserialize)]
struct Task {
    id: String,
    status: TaskStatus,
    #[serde(default)]
    artifacts: Vec<Artifact>,
}

#[derive(Deserialize)]
struct TaskStatus {
    state: TaskState,
    message: Option<AgentMessage>,
}

#[derive(Deserialize)]
enum TaskState {
    #[serde(rename = "TASK_STATE_SUBMITTED")]
    Submitted,
    #[serde(rename = "TASK_STATE_WORKING")]
    Working,
    #[serde(rename = "TASK_STATE_COMPLETED")]
    Completed,
    #[serde(rename = "TASK_STATE_FAILED")]
    Failed,
    #[serde(rename = "TASK_STATE_CANCELED")]
    Canceled,
    #[serde(rename = "TASK_STATE_REJECTED")]
    Rejected,
    #[serde(rename = "TASK_STATE_INPUT_REQUIRED")]
    InputRequired,
    #[serde(rename = "TASK_STATE_AUTH_REQUIRED")]
    AuthRequired,
}

fn reply_text<'a>(parts: impl Iterator<Item = &'a Part>) -> Result<String> {
    let body = parts
        .filter_map(|part| part.text.as_deref())
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if body.is_empty() || body.len() > MAX_REPLY_BYTES {
        return Err("a2a_reply_requires_supported_text");
    }
    Ok(body)
}

impl AgentMessage {
    fn answer(&self) -> Result<String> {
        if self.role != "ROLE_AGENT" {
            return Err("a2a_reply_role_mismatch");
        }
        reply_text(self.parts.iter())
    }
}

impl Task {
    fn answer(&self) -> Result<String> {
        let parts = || self.artifacts.iter().flat_map(|artifact| &artifact.parts);
        if parts().any(|part| part.text.is_some()) {
            return reply_text(parts());
        }
        self.status
            .message
            .as_ref()
            .ok_or("a2a_reply_requires_supported_text")?
            .answer()
    }
}

impl Agent {
    fn new(binding: &Binding) -> Result<Self> {
        let endpoint = Url::parse(&binding.endpoint).map_err(|_| "invalid_a2a_endpoint")?;
        let loopback = endpoint
            .host_str()
            .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback());
        if !matches!(endpoint.scheme(), "http" | "https")
            || (endpoint.scheme() == "http" && !loopback)
            || endpoint.host().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || binding
                .agent_token_file
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
        {
            return Err("invalid_a2a_endpoint_or_credential_path");
        }
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(8))
            .build()
            .map_err(|_| "a2a_client_unavailable")?;
        let token = binding
            .agent_token_file
            .as_deref()
            .map(crate::stdio_mcp::bearer)
            .transpose()
            .map_err(|_| "a2a_credential_unavailable")?;
        Ok(Self {
            client,
            endpoint,
            token,
        })
    }

    fn rpc<T: DeserializeOwned>(&self, id: u8, method: &str, params: Value) -> Result<T> {
        let mut request = self
            .client
            .post(self.endpoint.clone())
            .header("A2A-Version", "1.0")
            .json(&json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params}));
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        let response = request.send().map_err(|_| "a2a_transport_uncertain")?;
        if !response.status().is_success() {
            return Err("a2a_request_rejected");
        }
        let mut bytes = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "a2a_transport_uncertain")?;
        if bytes.len() as u64 > MAX_RESPONSE_BYTES {
            return Err("a2a_response_too_large");
        }
        let reply = mcp_request::strict_json(&bytes).map_err(|_| "invalid_a2a_response")?;
        if reply["jsonrpc"] != "2.0" || reply["id"] != id || reply.get("error").is_some() {
            return Err("a2a_request_rejected");
        }
        serde_json::from_value(reply["result"].clone()).map_err(|_| "invalid_a2a_response")
    }

    fn send(&self, message_id: &str, body: &str) -> Result<Submission> {
        self.rpc(1, "SendMessage", json!({
            "message":{"messageId":message_id,"role":"ROLE_USER","parts":[{"text":body}]},
            "configuration":{"returnImmediately":true,"historyLength":0,"acceptedOutputModes":["text/plain"]}
        }))
    }

    fn get_task(&self, id: &str) -> Result<Task> {
        let task: Task = self.rpc(2, "GetTask", json!({"id":id,"historyLength":0}))?;
        if task.id != id {
            return Err("a2a_task_id_mismatch");
        }
        Ok(task)
    }
}

struct Receiver<'a> {
    hub: &'a dyn Transport,
    profile: String,
}

fn digest(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "invalid_a2a_payload")?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

impl Receiver<'_> {
    fn call<T: DeserializeOwned>(&self, action: Action) -> Result<T> {
        let value = self.hub.call(Request {
            recipient: MAILBOX.into(),
            session_id: MAILBOX.into(),
            profile_id: self.profile.clone(),
            action,
        })?;
        serde_json::from_value(value).map_err(|_| "invalid_receiver_response")
    }

    fn renew(&self, delivery: &Delivery) -> Result<()> {
        self.call::<i64>(Action::Renew {
            message_id: delivery.message.id.clone(),
            receipt: delivery.receipt.clone(),
            lease_seconds: LEASE_SECONDS,
        })?;
        Ok(())
    }

    fn complete(&self, delivery: &Delivery, body: Option<String>, failure: bool) -> Result<()> {
        if !failure && let Some(body) = &body {
            self.call::<Record>(Action::Reconcile {
                message_id: delivery.message.id.clone(),
                receipt: delivery.receipt.clone(),
                answer_digest: digest(body)?,
            })?;
        }
        self.call::<Value>(Action::Complete {
            message_id: delivery.message.id.clone(),
            receipt: delivery.receipt.clone(),
            body,
            failure,
        })?;
        Ok(())
    }

    fn process(&self, agent: &Agent) -> Result<bool> {
        let Some(delivery) = self.call::<Option<Delivery>>(Action::Claim {
            lease_seconds: LEASE_SECONDS,
        })?
        else {
            return Ok(false);
        };
        let message = &delivery.message;
        if message.envelope.in_reply_to.is_some() {
            self.complete(&delivery, None, true)?;
            return Ok(true);
        }
        let mut task = if delivery.attempt == 1 {
            let mut record = self.call::<Record>(Action::Begin {
                message_id: message.id.clone(),
                receipt: delivery.receipt.clone(),
                payload_digest: digest(&message.envelope.body)?,
            })?;
            self.renew(&delivery)?;
            match agent.send(&record.input_id, &message.envelope.body)? {
                Submission::Message(reply) => {
                    self.complete(&delivery, Some(reply.answer()?), false)?;
                    return Ok(true);
                }
                Submission::Task(task) => {
                    record.admission_id = Some(task.id.clone());
                    record.stage = "admitted".into();
                    self.call::<Record>(Action::Update {
                        receipt: delivery.receipt.clone(),
                        record: Box::new(record),
                    })?;
                    task
                }
            }
        } else {
            let record = self
                .call::<Record>(Action::Load {
                    message_id: message.id.clone(),
                    receipt: delivery.receipt.clone(),
                })
                .map_err(|_| "a2a_submission_requires_reconciliation")?;
            let id = record
                .admission_id
                .ok_or("a2a_submission_requires_reconciliation")?;
            self.renew(&delivery)?;
            agent.get_task(&id)?
        };
        loop {
            match task.status.state {
                TaskState::Submitted | TaskState::Working => {
                    thread::sleep(POLL_INTERVAL);
                    self.renew(&delivery)?;
                    task = agent.get_task(&task.id)?;
                }
                TaskState::Completed => {
                    self.complete(&delivery, Some(task.answer()?), false)?;
                    return Ok(true);
                }
                TaskState::InputRequired | TaskState::AuthRequired => {
                    return Err("a2a_task_requires_action_at_the_agent");
                }
                TaskState::Failed | TaskState::Canceled | TaskState::Rejected => {
                    self.complete(
                        &delivery,
                        Some("The A2A agent ended the task without an answer.".into()),
                        true,
                    )?;
                    return Ok(true);
                }
            }
        }
    }
}

pub(crate) fn run(options: &Options) -> Result<()> {
    let binding = Binding::read(&options.binding)?;
    let agent = Agent::new(&binding)?;
    let hub = HubClient::new(binding.hub_port, binding.hub_token_file.clone())?;
    let receiver = Receiver {
        hub: &hub,
        profile: digest(&binding)?,
    };
    loop {
        let worked = receiver.process(&agent)?;
        if options.once {
            return Ok(());
        }
        if !worked {
            thread::sleep(POLL_INTERVAL);
        }
    }
}
