use lettre::{
    Address, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, message::Mailbox,
    transport::smtp::authentication::Credentials,
};
use serde::Deserialize;
use std::time::Duration;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SmtpSettings {
    host: String,
    port: u16,
    username: String,
    from: String,
}

pub(crate) struct SmtpMailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
}

pub(crate) enum NoticeDeliveryError {
    TryAgain,
    Failed,
}

impl SmtpMailer {
    pub(crate) fn new(settings: SmtpSettings, password: &str) -> Option<Self> {
        let from = settings.from.parse().ok()?;
        if settings.username.is_empty() || password.is_empty() || settings.port == 0 {
            return None;
        }
        let transport = AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&settings.host)
            .ok()?
            .port(settings.port)
            .credentials(Credentials::new(settings.username, password.to_owned()))
            .timeout(Some(Duration::from_secs(15)))
            .build();
        Some(Self { transport, from })
    }

    pub(crate) async fn send_code(&self, email: &str, code: &str) -> Option<()> {
        let message = Message::builder()
            .from(self.from.clone())
            .to(email.parse().ok()?)
            .subject("Your 999K verification code")
            .body(format!(
                "Your one-time 999K verification code is {code}.\n\nUse it to reserve your hosted address."
            ))
            .ok()?;
        self.transport.send(message).await.ok()?;
        Some(())
    }

    pub(crate) async fn send_issuance_notice(
        &self,
        recipient: &Address,
        issued_at: i64,
    ) -> Result<(), NoticeDeliveryError> {
        let issued_at =
            chrono::DateTime::from_timestamp(issued_at, 0).ok_or(NoticeDeliveryError::Failed)?;
        let issued_at = issued_at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let message = Message::builder()
            .from(self.from.clone())
            .to(Mailbox::new(None, recipient.clone()))
            .subject("999K hosted address issued")
            .body(format!("A hosted address was issued at {issued_at}.\n"))
            .map_err(|_| NoticeDeliveryError::Failed)?;
        self.transport.send(message).await.map_err(|error| {
            if error.is_transient() {
                NoticeDeliveryError::TryAgain
            } else {
                NoticeDeliveryError::Failed
            }
        })?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn use_loopback_fixture(&mut self, port: u16) {
        self.transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
            .port(port)
            .timeout(Some(Duration::from_secs(5)))
            .build();
    }
}

pub(crate) fn valid_otp(value: &str) -> bool {
    (6..=8).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub(crate) fn normalized_email(value: &str) -> Option<String> {
    let value = value.trim();
    let (local, domain) = value.split_once('@')?;
    if value.len() > 254
        || local.is_empty()
        || domain.is_empty()
        || !domain.contains('.')
        || !local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".!#$%&*+-/=?^_`{|}~".contains(&b))
        || !domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
    {
        return None;
    }
    Some(value.to_ascii_lowercase())
}
