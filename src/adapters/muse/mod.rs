//! Per-agent compatibility surface for the shared deferred mailbox engine.
use crate::deferred_mailbox::{self as engine, Mailbox};
use serde::Serialize;
use std::path::Path;
pub const MAX_REQUEST_BYTES: usize = 16384;
pub const MAX_REPLY_BYTES: usize = 16384;
/// Host checks authenticated principal, mailbox and operation before constructing this.
/// Deliberately not deserializable from client JSON.
#[derive(Clone, Debug)]
pub struct MuseScope {
    pub allowed_senders: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{reason}")]
pub struct MailboxError {
    pub reason: &'static str,
}
impl MailboxError {
    pub fn new(reason: &'static str) -> Self {
        Self { reason }
    }
}
impl From<rusqlite::Error> for MailboxError {
    fn from(_: rusqlite::Error) -> Self {
        Self::new("storage_unavailable")
    }
}
pub type Result<T> = std::result::Result<T, MailboxError>;
#[derive(Debug, Serialize)]
pub struct ObservedMessage {
    pub id: String,
    pub message_id: String,
    pub conversation_id: String,
    pub sender: String,
    pub recipient: &'static str,
    pub body: String,
    pub correlation_id: String,
    pub in_reply_to: Option<String>,
    pub state: &'static str,
    pub reply_id: Option<String>,
}
#[derive(Debug, Serialize)]
pub struct Completion {
    pub state: &'static str,
    pub reply_id: String,
    pub duplicate: bool,
}
/// A bounded internal scan page. Never serialize this as client cursor progress.
#[derive(Debug)]
pub struct ScanPage {
    pub message: Option<ObservedMessage>,
    pub next_seq: i64,
    pub scanned: usize,
}
pub const SCAN_PAGE_SIZE: usize = 100;

impl From<engine::MailboxError> for MailboxError {
    fn from(error: engine::MailboxError) -> Self {
        Self::new(error.reason)
    }
}
impl From<engine::ObservedMessage> for ObservedMessage {
    fn from(message: engine::ObservedMessage) -> Self {
        Self {
            message_id: message.id.clone(),
            id: message.id,
            conversation_id: message.conversation_id,
            sender: message.sender,
            recipient: message.recipient,
            body: message.body,
            correlation_id: message.correlation_id,
            in_reply_to: message.in_reply_to,
            state: message.state,
            reply_id: message.reply_id,
        }
    }
}
impl From<engine::Completion> for Completion {
    fn from(completion: engine::Completion) -> Self {
        Self {
            state: completion.state,
            reply_id: completion.reply_id,
            duplicate: completion.duplicate,
        }
    }
}
impl From<engine::ScanPage> for ScanPage {
    fn from(page: engine::ScanPage) -> Self {
        Self {
            message: page.message.map(Into::into),
            next_seq: page.next_seq,
            scanned: page.scanned,
        }
    }
}
pub fn prepare(p: impl AsRef<Path>) -> Result<()> {
    Mailbox::Muse.prepare(p).map_err(Into::into)
}
pub fn is_prepared(p: impl AsRef<Path>) -> Result<bool> {
    Mailbox::Muse.is_prepared(p).map_err(Into::into)
}
pub fn read(p: impl AsRef<Path>, id: &str, s: &MuseScope) -> Result<ObservedMessage> {
    Mailbox::Muse
        .read(p, id, &s.allowed_senders)
        .map(Into::into)
        .map_err(Into::into)
}
pub fn scan(p: impl AsRef<Path>, s: &MuseScope) -> Result<Option<ObservedMessage>> {
    Mailbox::Muse
        .scan(p, &s.allowed_senders)
        .map(|message| message.map(Into::into))
        .map_err(Into::into)
}
pub fn scan_page(
    p: impl AsRef<Path>,
    s: &MuseScope,
    after_seq: i64,
    terminal: bool,
) -> Result<ScanPage> {
    Mailbox::Muse
        .scan_page(p, &s.allowed_senders, after_seq, terminal)
        .map(Into::into)
        .map_err(Into::into)
}
pub fn reply(
    p: impl AsRef<Path>,
    id: &str,
    text: &str,
    mut authorize: impl FnMut() -> Result<MuseScope>,
) -> Result<Completion> {
    Mailbox::Muse
        .reply(p, id, text, || {
            authorize()
                .map(|scope| scope.allowed_senders)
                .map_err(|error| engine::MailboxError::new(error.reason))
        })
        .map(Into::into)
        .map_err(Into::into)
}
