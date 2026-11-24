//! Validate an operator's triage record, then select only supported local tools.
use super::{Capability, error, owner_directory};
use radio999k::{mcp_request::strict_json, private_file};
use serde_json::Value;
use std::{
    collections::HashSet,
    io::{self, Read},
    path::Path,
};

pub(super) const DIRECTORY: &str = "triage";
const SCHEMA: &str = include_str!("../../schemas/triage.schema.json");

pub(super) fn valid_slug(name: &str) -> bool {
    name.len() <= 48
        && name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !name.ends_with('-')
}

// Checks the property shapes in the bundled triage schema. This is not a
// general JSON Schema validator.
fn matches_property(value: &Value, rule: &Value) -> bool {
    let Some(fields) = rule.as_object() else {
        return false;
    };
    let known = |expected: &[&str]| {
        fields.keys().all(|key| {
            matches!(key.as_str(), "type" | "description") || expected.contains(&key.as_str())
        })
    };
    match rule["type"].as_str() {
        Some("string") if known(&["enum", "minLength", "maxLength", "pattern"]) => {
            let Some(text) = value.as_str() else {
                return false;
            };
            let length = text.chars().count() as u64;
            rule["enum"]
                .as_array()
                .is_none_or(|choices| choices.contains(value))
                && rule["minLength"].as_u64().is_none_or(|min| length >= min)
                && rule["maxLength"].as_u64().is_none_or(|max| length <= max)
                && rule["pattern"]
                    .as_str()
                    .is_none_or(|p| p == "^[a-z](?:[a-z0-9-]{0,46}[a-z0-9])?$" && valid_slug(text))
        }
        Some("array") if known(&["items", "minItems", "uniqueItems"]) => {
            let Some(items) = value.as_array() else {
                return false;
            };
            let mut seen = HashSet::new();
            rule["minItems"]
                .as_u64()
                .is_none_or(|min| items.len() as u64 >= min)
                && items.iter().all(|item| {
                    item.as_str().is_some_and(|text| {
                        matches_property(item, &rule["items"])
                            && (rule["uniqueItems"] != true || seen.insert(text))
                    })
                })
        }
        _ => false,
    }
}

fn record(bytes: &[u8]) -> io::Result<Value> {
    let invalid = || error("invalid triage record; follow schemas/triage.schema.json");
    let schema: Value = serde_json::from_str(SCHEMA).map_err(|_| invalid())?;
    let root = schema.as_object().ok_or_else(invalid)?;
    if schema["type"] != "object"
        || schema["additionalProperties"] != false
        || root.keys().any(|key| {
            !matches!(
                key.as_str(),
                "$schema"
                    | "title"
                    | "description"
                    | "type"
                    | "additionalProperties"
                    | "required"
                    | "properties"
            )
        })
    {
        return Err(invalid());
    }
    let fields = schema["properties"].as_object().ok_or_else(invalid)?;
    let required = schema["required"].as_array().ok_or_else(invalid)?;
    let value = strict_json(bytes).map_err(|_| invalid())?;
    let input = value.as_object().ok_or_else(invalid)?;
    if !required
        .iter()
        .all(|key| key.as_str().is_some_and(|key| input.contains_key(key)))
        || !input.iter().all(|(key, value)| {
            fields
                .get(key)
                .is_some_and(|rule| matches_property(value, rule))
        })
    {
        return Err(invalid());
    }
    Ok(value)
}

pub(super) fn read(home: &Path, name: &str) -> io::Result<Option<Capability>> {
    if !home.is_absolute() {
        return Err(error("an explicit absolute local mailbox home is required"));
    }
    let home = match owner_directory(home) {
        Ok(home) => home,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    let directory = match owner_directory(&home.join(DIRECTORY)) {
        Ok(directory) => directory,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(error("triage requires an owner-private directory")),
    };
    let path = directory.join(format!("{name}.json"));
    match path.symlink_metadata() {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => {
            result?;
        }
    }
    let input = private_file::open_private_file(&path)
        .map_err(|_| error("triage requires an owner-private regular file"))?;
    let mut bytes = Vec::new();
    input
        .take(private_file::MAX_PRIVATE_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > private_file::MAX_PRIVATE_CONFIG_BYTES {
        return Err(error(
            "triage record exceeds the private configuration size limit",
        ));
    }
    let value = record(&bytes)?;
    if value["agent"] != name {
        return Err(error("triage agent must match the requested mailbox"));
    }
    if value["adapter_path"] != "local_mcp"
        || value["initiator"] != "agent"
        || value["transport"] != "streamable_http"
        || value["auth"] != "bearer"
    {
        return Err(error(
            "triage can stage only agent-initiated local MCP with Streamable HTTP and Bearer; verify remote support separately",
        ));
    }
    if value["wake"] != "none" {
        return Err(error(
            "local triage requires wake:none; webhook or unknown wake is not staged",
        ));
    }
    if !value["send_to"].as_array().is_some_and(Vec::is_empty) {
        return Err(error(
            "local triage cannot grant peer access; use an empty send_to list",
        ));
    }
    let capability = value["operations"]
        .as_array()
        .and_then(|operations| Capability::for_operations(operations))
        .ok_or_else(|| {
            error("local triage supports only read and wait; reply, send and ack are not staged")
        })?;
    Ok(Some(capability))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_candidate_is_valid_evidence_not_runtime_authority() {
        let sample = record(include_bytes!("../../schemas/examples/vooy.json")).unwrap();
        assert_eq!(sample["evidence"], "candidate");
        assert_eq!(sample["auth"], "unknown");
    }
}
