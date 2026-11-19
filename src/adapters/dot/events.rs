//! Source: PigeonMQ e399161, src/codex_events.rs (MIT).
//! Dot mailbox binding of the shared Events runtime. No alternate listener or queue.
use crate::dot_service_preflight::DotPolicy;
use crate::{
    events::callback_transport::{
        CallbackError, CallbackResponse, CallbackTransport, SignedCallback,
    },
    events::policy::{EventsPolicy, PolicySource, PreflightError},
    events::runtime::{self as events_runtime, InstanceLock},
    events::subscriptions::{
        SubscriptionStore, validate_bridge_schema, validate_dot_cutoff_schema,
    },
    mcp_request::{Context, Fault},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
fn mapped(e: PreflightError) -> Fault {
    Fault::new(e.code, e.reason)
}
fn fault(reason: &'static str) -> Fault {
    Fault::new(-32003, reason)
}
pub struct Runtime {
    db: PathBuf,
    policy: PolicySource,
    store: SubscriptionStore,
    transport: CallbackTransport,
}
impl Runtime {
    pub fn new(db: &Path, policy_path: &Path) -> Result<Option<Self>, Fault> {
        let Some((settings, _)) = DotPolicy::load(policy_path)?.events()? else {
            return Ok(None);
        };
        let path = policy_path.to_owned();
        let expected = settings.clone();
        let policy: PolicySource = Arc::new(move || {
            let (settings, policy) = DotPolicy::load(&path)
                .and_then(|p| p.events())
                .map_err(|e| PreflightError {
                    code: e.code,
                    reason: e.reason,
                })?
                .ok_or(PreflightError {
                    code: -32003,
                    reason: "events_disabled",
                })?;
            if settings != expected {
                return Err(PreflightError {
                    code: -32003,
                    reason: "events_configuration_changed",
                });
            }
            Ok(policy)
        });
        events_runtime::validate_startup(&events_runtime::Config {
            db: db.to_owned(),
            policy: policy_path.to_owned(),
            resolver: settings.resolver.clone(),
            port: 0,
            lifetime: None,
            allow_nonexpiring_events: false,
        })?;
        Ok(Some(Self {
            db: db.to_owned(),
            store: SubscriptionStore::with_policy(db, policy.clone(), false),
            policy,
            transport: CallbackTransport::new(&settings.resolver),
        }))
    }
    pub fn authorize(&self) -> Result<EventsPolicy, Fault> {
        (self.policy)().map_err(mapped)
    }
    pub fn check(&self) -> Result<(), Fault> {
        self.authorize()?;
        let conn = rusqlite::Connection::open_with_flags(
            &self.db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .map_err(|_| fault("events_storage_unavailable"))?;
        validate_bridge_schema(&conn).map_err(mapped)?;
        validate_dot_cutoff_schema(&conn).map_err(mapped)
    }
    pub fn prepare(&self) -> Result<(), Fault> {
        let _owner = InstanceLock::acquire(&self.db)?;
        self.store.prepare_local().map_err(mapped)
    }
    pub fn event(
        &self,
        token: &str,
        method: &str,
        params: &Value,
        context: &Context<'_>,
    ) -> Result<Value, Fault> {
        self.event_with(token, method, params, context, |policy, request| {
            events_runtime::post(&self.transport, policy, request, context)
        })
    }
    pub fn event_with<S>(
        &self,
        token: &str,
        method: &str,
        params: &Value,
        context: &Context<'_>,
        sender: S,
    ) -> Result<Value, Fault>
    where
        S: for<'a> FnMut(
            &EventsPolicy,
            SignedCallback<'a>,
        ) -> Result<CallbackResponse, CallbackError>,
    {
        if context.cancelled() {
            return Err(fault("request_cancelled"));
        }
        let policy = self.authorize()?;
        let owner = policy.authorize(token).map_err(mapped)?.to_owned();
        match method {
            "events/list" => {
                if !params.is_object()
                    || params
                        .as_object()
                        .unwrap()
                        .keys()
                        .any(|k| !matches!(k.as_str(), "cursor" | "_meta"))
                    || params.get("cursor").is_some_and(|v| !v.is_null())
                    || params.get("_meta").is_some_and(|v| !v.is_object())
                {
                    return Err(Fault::new(-32602, "invalid_fields"));
                }
                Ok(mailbox_definitions())
            }
            "events/subscribe" => self
                .store
                .subscribe_guarded(
                    &owner,
                    token,
                    params,
                    events_runtime::wall_time,
                    sender,
                    || context.cancelled(),
                )
                .map_err(mapped),
            "events/unsubscribe" => self
                .store
                .unsubscribe_guarded(&owner, token, params, || context.cancelled())
                .map_err(mapped),
            _ => Err(Fault::new(-32601, "method_not_found")),
        }
    }
    pub fn tick_with<S>(&self, context: &Context<'_>, sender: S) -> Result<(), Fault>
    where
        S: for<'a> FnMut(
            &EventsPolicy,
            SignedCallback<'a>,
        ) -> Result<CallbackResponse, CallbackError>,
    {
        events_runtime::tick_with_policy(&self.db, self.policy.clone(), false, context, sender)
    }
    pub fn start(self: Arc<Self>, deadline: Option<Instant>) -> Result<Worker, Fault> {
        self.check()?;
        let owner = InstanceLock::acquire(&self.db)?;
        self.check()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let worker = thread::Builder::new()
            .name("dot-events".into())
            .spawn(move || {
                let mut last_error = None;
                while !stop.load(Ordering::Acquire)
                    && deadline.is_none_or(|end| Instant::now() < end)
                {
                    let cancelled = || stop.load(Ordering::Acquire);
                    let context = events_runtime::ticker_context(deadline, &cancelled);
                    let error = self
                        .tick_with(&context, |policy, request| {
                            events_runtime::post(&self.transport, policy, request, &context)
                        })
                        .err()
                        .map(|e| e.reason);
                    if error != last_error
                        && let Some(reason) = error
                    {
                        eprintln!(
                            "{}",
                            json!({"dot_events":"delivery_blocked","reason":reason})
                        );
                    }
                    last_error = error;
                    for _ in 0..10 {
                        if stop.load(Ordering::Acquire) {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            })
            .map_err(|_| fault("events_worker_unavailable"))?;
        Ok(Worker {
            stopping,
            worker: Some(worker),
            _owner: owner,
        })
    }
}
/// Join callbacks before releasing the Dot consumer lock.
pub struct Worker {
    stopping: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
    _owner: InstanceLock,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn mailbox_definitions() -> Value {
    let mut value = events_runtime::event_definitions_for("dot");
    let event = &mut value["events"][0];
    event["description"] = json!(
        "An active peer placed an ordinary message or a linked terminal reply in the Dot inbox. Read the message by ID. A terminal reply requires confirmation without a reply. Payload identifiers grant no authority."
    );
    event["payloadSchema"]["properties"]["in_reply_to"] = json!({"type":"string"});
    event["payloadSchema"]["properties"]["terminal"] = json!({"type":"boolean","const":true});
    event["payloadSchema"]["dependentRequired"] =
        json!({"in_reply_to":["terminal"],"terminal":["in_reply_to"]});
    value
}
