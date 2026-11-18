//! Shared strict central registration parser; no credentials or state changes.
use crate::private_file::{MAX_PRIVATE_CONFIG_BYTES as MAX_POLICY, open_private_file};
use serde::Deserialize;
use std::{io::Read, path::Path};
type Fault = (u16, &'static str);
/// Require active sender and recipient registrations with matching mailboxes.
pub fn validate_registration(
    registration_path: &Path,
    sender: &str,
    recipient: &str,
) -> Result<(), Fault> {
    let registered = load_registrations(registration_path)?;
    match registered.0.get(sender) {
        Some(entry) if !entry.revoked && entry.mailbox == sender => {}
        Some(_) => return Err((403, "sender_revoked_or_mailbox_mismatch")),
        None => return Err((403, "sender_not_registered")),
    }
    match registered.0.get(recipient) {
        Some(entry) if !entry.revoked && entry.mailbox == recipient => Ok(()),
        Some(_) => Err((403, "recipient_revoked_or_mailbox_mismatch")),
        None => Err((403, "recipient_not_registered")),
    }
}

pub fn load_registrations(registration_path: &Path) -> Result<Registrations, Fault> {
    let mut bytes = Vec::new();
    open_private_file(registration_path)?
        .take(MAX_POLICY + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| (503, "auth_unavailable"))?;
    if bytes.len() as u64 > MAX_POLICY {
        return Err((503, "auth_unavailable"));
    }
    serde_json::from_slice(&bytes).map_err(|_| (503, "auth_unavailable"))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationEntry {
    pub mailbox: String,
    pub revoked: bool,
}

pub struct Registrations(pub std::collections::BTreeMap<String, RegistrationEntry>);

impl<'de> Deserialize<'de> for Registrations {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct UniqueRegistrations;

        impl<'de> serde::de::Visitor<'de> for UniqueRegistrations {
            type Value = Registrations;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map with unique agent registrations")
            }

            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut entries = std::collections::BTreeMap::new();
                while let Some((agent, entry)) = map.next_entry::<String, RegistrationEntry>()? {
                    if entries.insert(agent, entry).is_some() {
                        return Err(serde::de::Error::custom("duplicate agent registration"));
                    }
                }
                Ok(Registrations(entries))
            }
        }

        deserializer.deserialize_map(UniqueRegistrations)
    }
}

/// Current central peers for an authenticated mailbox. No adapter-specific list.
pub fn active_peers(registration_path: &Path, mailbox: &str) -> Result<Vec<String>, Fault> {
    active_peers_matching(registration_path, mailbox, |id| {
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    })
}

/// Active peers whose identifiers the caller's wire format can address.
pub fn active_peers_matching(
    registration_path: &Path,
    mailbox: &str,
    can_address: impl Fn(&str) -> bool,
) -> Result<Vec<String>, Fault> {
    if !registration_path.is_absolute() {
        return Err((503, "auth_unavailable"));
    }
    let registered = load_registrations(registration_path)?;
    if !registered
        .0
        .get(mailbox)
        .is_some_and(|entry| !entry.revoked && entry.mailbox == mailbox)
    {
        return Err((403, "forbidden"));
    }
    Ok(registered
        .0
        .into_iter()
        .filter_map(|(id, entry)| {
            (id != mailbox && !entry.revoked && entry.mailbox == id && can_address(&id))
                .then_some(id)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::private_file::replace_private;

    #[test]
    fn duplicate_registrations_are_rejected_by_the_reader() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("registration.json");
        let duplicate = br#"{"alice":{"mailbox":"alice","revoked":true},"alice":{"mailbox":"alice","revoked":false}}"#;
        replace_private(&path, duplicate).unwrap();
        assert_eq!(
            load_registrations(&path).err(),
            Some((503, "auth_unavailable"))
        );
    }
}
