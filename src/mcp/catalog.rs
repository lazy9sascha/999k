//! Packaged deployment identities and defaults. These never enable routes or grant authority.
use serde::Deserialize;
use std::{collections::HashSet, sync::LazyLock};

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EventsCapability {
    #[default]
    None,
    Codex,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub mailbox: &'static str,
    pub server_name: &'static str,
    pub events: EventsCapability,
}

/// Runtime behavior only. The deployment name, route, and policy come from data.
#[derive(Clone, Copy, Debug, Deserialize, Hash, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Adapter {
    Muse,
    Dot,
    Grok,
    SharedMailbox,
    Receiver,
}

impl Adapter {
    pub fn requires_registration(self) -> bool {
        matches!(self, Self::SharedMailbox | Self::Receiver)
    }
}

pub const fn default_body_limit(max_headers: usize) -> usize {
    6 * 16384 + max_headers
}

#[derive(Debug, PartialEq, Eq)]
pub struct HttpProfile {
    pub id: &'static str,
    pub adapter: Adapter,
    pub route: &'static str,
    pub policy_file: &'static str,
    pub enabled_key: &'static str,
    pub endpoint_key: Option<&'static str>,
    pub manifest_only: bool,
    pub mailbox: Option<Profile>,
}

impl HttpProfile {
    pub const fn body_limit(&self, max_headers: usize) -> usize {
        match self.adapter {
            Adapter::Dot => crate::dot_mcp_host::MAX_BODY,
            Adapter::Grok => 6 * crate::grok_mailbox::MAX_TEXT + max_headers,
            _ => default_body_limit(max_headers),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry<'a> {
    id: &'a str,
    adapter: Adapter,
    route: &'a str,
    policy_file: &'a str,
    enabled_key: &'a str,
    #[serde(borrow)]
    endpoint_key: Option<&'a str>,
    #[serde(borrow)]
    server_name: Option<&'a str>,
    #[serde(default)]
    manifest_only: bool,
    #[serde(default)]
    events: EventsCapability,
}

static HTTP_PROFILES: LazyLock<Vec<HttpProfile>> = LazyLock::new(|| {
    parse_registry(include_str!("built_in_profiles.json"))
        .expect("invalid packaged profile registry")
});

fn parse_registry(data: &'static str) -> Option<Vec<HttpProfile>> {
    let entries: Vec<Entry<'static>> = serde_json::from_str(data).ok()?;
    let (mut ids, mut routes, mut files, mut keys, mut singletons) = (
        HashSet::new(),
        HashSet::new(),
        HashSet::new(),
        HashSet::new(),
        HashSet::new(),
    );
    let mut codex_events = false;
    let mut profiles = Vec::new();
    for entry in entries {
        let adapter = entry.adapter;
        if entry.id.is_empty()
            || if adapter == Adapter::Receiver {
                entry.route != "/receiver"
            } else {
                !valid_mcp_route(entry.route)
            }
            || !ids.insert(entry.id)
            || !routes.insert(entry.route)
            || !files.insert(entry.policy_file)
            || !keys.insert(entry.enabled_key)
            || entry.endpoint_key.is_some_and(|key| !keys.insert(key))
            || adapter != Adapter::SharedMailbox && !singletons.insert(adapter)
        {
            return None;
        }
        let mailbox = match (adapter, entry.server_name) {
            (Adapter::SharedMailbox, Some(server_name)) if !server_name.is_empty() => {
                if entry.events == EventsCapability::Codex
                    && std::mem::replace(&mut codex_events, true)
                {
                    return None;
                }
                Some(Profile {
                    mailbox: entry.id,
                    server_name,
                    events: entry.events,
                })
            }
            (Adapter::SharedMailbox, _) | (_, Some(_)) => return None,
            (_, None) if entry.events != EventsCapability::None => return None,
            _ => None,
        };
        profiles.push(HttpProfile {
            id: entry.id,
            adapter,
            route: entry.route,
            policy_file: entry.policy_file,
            enabled_key: entry.enabled_key,
            endpoint_key: entry.endpoint_key,
            manifest_only: entry.manifest_only,
            mailbox,
        });
    }
    (!profiles.is_empty()).then_some(profiles)
}

pub fn valid_mcp_route(route: &str) -> bool {
    let Some(slug) = route.strip_prefix('/').and_then(|r| r.strip_suffix("/mcp")) else {
        return false;
    };
    slug.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && slug
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !slug.ends_with('-')
        && slug != "mailbox"
}

pub fn http_profiles() -> &'static [HttpProfile] {
    &HTTP_PROFILES
}

pub fn http_by_id(id: &str) -> Option<&'static HttpProfile> {
    http_profiles().iter().find(|profile| profile.id == id)
}

pub fn http_by_route(route: &str) -> Option<&'static HttpProfile> {
    http_profiles()
        .iter()
        .find(|profile| profile.route == route)
}

pub fn by_adapter(adapter: Adapter) -> &'static HttpProfile {
    assert_ne!(adapter, Adapter::SharedMailbox);
    http_profiles()
        .iter()
        .find(|profile| profile.adapter == adapter)
        .expect("missing packaged adapter")
}

pub fn codex_http() -> &'static HttpProfile {
    http_profiles()
        .iter()
        .find(|profile| {
            profile
                .mailbox
                .is_some_and(|mailbox| mailbox.events == EventsCapability::Codex)
        })
        .expect("missing packaged Codex Events profile")
}

pub fn codex() -> &'static Profile {
    codex_http()
        .mailbox
        .as_ref()
        .expect("Events require a mailbox")
}

pub fn by_mailbox(mailbox: &str) -> Option<&'static Profile> {
    http_by_id(mailbox)?.mailbox.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_registry_preserves_installed_bindings() {
        for (id, route) in [
            ("muse", "/muse/mcp"),
            ("dot", "/dot/mcp"),
            ("grok-bot", "/grok/mcp"),
            ("codex-cloud", "/codex/mcp"),
            ("codex-local", "/receiver"),
        ] {
            let descriptor = http_by_id(id).unwrap();
            assert_eq!(http_by_route(route), Some(descriptor));
        }
        assert_eq!(codex().mailbox, "codex-cloud");
        assert_eq!(codex().server_name, "999k-codex");
        assert_eq!(codex().events, EventsCapability::Codex);
        assert!(by_mailbox("codex-local").is_none());
        assert!(http_by_route("/codex/mcp/").is_none());
    }

    #[test]
    fn new_mailbox_identity_and_route_are_registry_data() {
        let profiles = parse_registry(
            r#"[
            {"id":"example-agent","adapter":"shared-mailbox","route":"/example-agent/mcp",
             "policy_file":"example.json","enabled_key":"example_enabled","server_name":"example"}
        ]"#,
        )
        .unwrap();
        assert_eq!(profiles[0].mailbox.unwrap().mailbox, "example-agent");
        assert_eq!(profiles[0].mailbox.unwrap().events, EventsCapability::None);
        assert_eq!(profiles[0].route, "/example-agent/mcp");
    }
}
