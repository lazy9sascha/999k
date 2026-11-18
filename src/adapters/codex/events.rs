//! Source: PigeonMQ e399161, src/codex_events.rs (MIT).
//! Codex binding of the shared Events runtime. No alternate listener or queue.
use crate::{auth, codex_mcp::Profile};
use pigeonmq::mailbox_catalog as catalog;
use pigeonmq::{
    events::callback_transport::{
        CallbackError, CallbackResponse, CallbackTransport, SignedCallback,
    },
    events::policy::{EventsPolicy, PolicySource, PreflightError},
    events::runtime::{self as events_runtime, InstanceLock},
    events::subscriptions::{SubscriptionStore, validate_bridge_schema},
    mcp_request::{Context, Fault},
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
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
    settings: auth::Events,
    transport: CallbackTransport,
}
impl Runtime {
    pub fn new(profile: &Profile) -> Result<Option<Self>, (u16, &'static str)> {
        if profile.definition()?.events != catalog::EventsCapability::Codex {
            return Ok(None);
        }
        let Some((settings, _)) = auth::codex_events(&profile.policy, &profile.registration)?
        else {
            return Ok(None);
        };
        let policy_path = profile.policy.clone();
        let registration = profile.registration.clone();
        let expected = settings.clone();
        let policy: PolicySource = Arc::new(move || {
            let (settings, policy) = auth::codex_events(&policy_path, &registration)
                .map_err(|(_, reason)| PreflightError {
                    code: -32003,
                    reason,
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
            db: profile.db.clone(),
            policy: profile.policy.clone(),
            resolver: settings.resolver.clone(),
            port: 0,
            lifetime: None,
            allow_nonexpiring_events: settings.allow_nonexpiring,
        })
        .map_err(|e| (503, e.reason))?;
        Ok(Some(Self {
            db: profile.db.clone(),
            store: SubscriptionStore::with_policy(
                &profile.db,
                policy.clone(),
                settings.allow_nonexpiring,
            ),
            policy,
            transport: CallbackTransport::new(&settings.resolver),
            settings,
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
        validate_bridge_schema(&conn).map_err(mapped)
    }
    pub fn prepare(&self) -> Result<(), Fault> {
        let _owner = InstanceLock::acquire_codex(&self.db)?;
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
        self.authorize()?.authorize(token).map_err(mapped)?;
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
                Ok(events_runtime::event_definitions_for(
                    catalog::codex().mailbox,
                ))
            }
            "events/subscribe" => self
                .store
                .subscribe_guarded(
                    catalog::codex().mailbox,
                    token,
                    params,
                    events_runtime::wall_time,
                    sender,
                    || context.cancelled(),
                )
                .map_err(mapped),
            "events/unsubscribe" => self
                .store
                .unsubscribe_guarded(catalog::codex().mailbox, token, params, || {
                    context.cancelled()
                })
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
        events_runtime::tick_with_policy(
            &self.db,
            self.policy.clone(),
            self.settings.allow_nonexpiring,
            context,
            sender,
        )
    }
    pub fn start(self: Arc<Self>, deadline: Option<Instant>) -> Result<Worker, Fault> {
        self.check()?;
        let owner = InstanceLock::acquire_codex(&self.db)?;
        self.check()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let worker = thread::Builder::new()
            .name("codex-events".into())
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
                            json!({"codex_events":"delivery_blocked","reason":reason})
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
/// Join callbacks before releasing the shared Codex consumer lock.
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

#[cfg(test)]
#[path = "events_tests.rs"]
mod tests;
