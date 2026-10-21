// SPDX-License-Identifier: MIT OR Apache-2.0

//! Partial updates from the Settings page.
//!
//! A patch is a JSON object with the same shape as [`Settings`], containing only the fields that
//! change: `{"bandwidth": {"peerLimit": 80}}`. Unknown fields are rejected (they are typos or a
//! mismatched frontend), and the result is fully validated before it is accepted.

use serde_json::{Map, Value};

use crate::model::{FieldError, Settings};

/// Largest accepted patch, in bytes of JSON.
pub const MAX_PATCH_BYTES: usize = 64 * 1024;

/// Why a patch was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PatchError {
    /// The patch is not a JSON object.
    #[error("Settings changes must be a JSON object.")]
    NotObject,
    /// The patch is larger than [`MAX_PATCH_BYTES`].
    #[error("The settings change is too large.")]
    TooLarge,
    /// The patch names a setting that does not exist (or `version`, which is not editable).
    #[error("{0}: there is no such setting.")]
    UnknownField(String),
    /// A value has the wrong type (for example text where a number belongs).
    #[error("{0}: the value has the wrong type.")]
    WrongType(String),
    /// The merged settings failed validation.
    #[error("{}", first_message(.0))]
    Invalid(Vec<FieldError>),
}

fn first_message(errors: &[FieldError]) -> String {
    errors
        .first()
        .map_or_else(String::new, |e| format!("{}: {}", e.field, e.message))
}

/// Applies `patch` to `current`, returning the new, validated settings.
///
/// # Errors
///
/// Returns a [`PatchError`] if the patch is malformed or the result is invalid; `current` is
/// never modified.
pub fn apply_patch(current: &Settings, patch: &Value) -> Result<Settings, PatchError> {
    let Value::Object(changes) = patch else {
        return Err(PatchError::NotObject);
    };
    if serde_json::to_string(patch).map_or(true, |s| s.len() > MAX_PATCH_BYTES) {
        return Err(PatchError::TooLarge);
    }
    if changes.contains_key("version") {
        return Err(PatchError::UnknownField("version".to_owned()));
    }
    let Ok(Value::Object(mut doc)) = serde_json::to_value(current) else {
        return Err(PatchError::NotObject);
    };
    merge(&mut doc, changes, "")?;
    let merged = Value::Object(doc);
    let Ok(settings) = serde_json::from_value::<Settings>(merged.clone()) else {
        // serde_json's errors have no path, so find the section that fails on its own.
        let section = changes
            .keys()
            .find(|name| !section_parses(current, name, &merged[name.as_str()]))
            .cloned()
            .unwrap_or_default();
        return Err(PatchError::WrongType(section));
    };
    settings.validate().map_err(PatchError::Invalid)?;
    Ok(settings)
}

/// Copies `changes` into `doc`, recursing into objects; every key must already exist in `doc`.
fn merge(
    doc: &mut Map<String, Value>,
    changes: &Map<String, Value>,
    path: &str,
) -> Result<(), PatchError> {
    for (key, change) in changes {
        let field = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        let Some(slot) = doc.get_mut(key) else {
            return Err(PatchError::UnknownField(field));
        };
        match (slot, change) {
            (Value::Object(inner), Value::Object(inner_changes)) => {
                merge(inner, inner_changes, &field)?;
            }
            (Value::Object(_), _) => return Err(PatchError::WrongType(field)),
            (slot, _) => *slot = change.clone(),
        }
    }
    Ok(())
}

/// Whether `current` with section `name` replaced by `value` deserializes.
fn section_parses(current: &Settings, name: &str, value: &Value) -> bool {
    let Ok(Value::Object(mut doc)) = serde_json::to_value(current) else {
        return false;
    };
    doc.insert(name.to_owned(), value.clone());
    serde_json::from_value::<Settings>(Value::Object(doc)).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NetworkMode, SCHEMA_VERSION};
    use serde_json::json;

    #[test]
    fn applies_nested_changes_and_keeps_the_rest() {
        let current = Settings::default();
        let next = apply_patch(
            &current,
            &json!({"bandwidth": {"peerLimit": 80}, "network": {"mode": "socks5", "socks5": {"host": "10.0.0.1"}}}),
        )
        .unwrap();
        assert_eq!(next.bandwidth.peer_limit, 80);
        assert_eq!(next.network.mode, NetworkMode::Socks5);
        assert_eq!(next.network.socks5.host, "10.0.0.1");
        assert_eq!(next.network.socks5.port, 1080, "untouched fields are kept");
        assert_eq!(next.search, current.search);
        assert_eq!(next.version, SCHEMA_VERSION);
    }

    #[test]
    fn null_clears_optional_settings() {
        let mut current = Settings::default();
        current.storage.cache_limit_gb = Some(50);
        let next = apply_patch(&current, &json!({"storage": {"cacheLimitGb": null}})).unwrap();
        assert_eq!(next.storage.cache_limit_gb, None);
    }

    #[test]
    fn rejects_malformed_patches() {
        let s = Settings::default();
        assert_eq!(apply_patch(&s, &json!([1])), Err(PatchError::NotObject));
        assert_eq!(
            apply_patch(&s, &json!({"version": 2})),
            Err(PatchError::UnknownField("version".into()))
        );
        assert_eq!(
            apply_patch(&s, &json!({"bandwidth": {"peerLimt": 3}})),
            Err(PatchError::UnknownField("bandwidth.peerLimt".into()))
        );
        assert_eq!(
            apply_patch(&s, &json!({"network": 5})),
            Err(PatchError::WrongType("network".into()))
        );
        assert_eq!(
            apply_patch(&s, &json!({"bandwidth": {"peerLimit": "many"}})),
            Err(PatchError::WrongType("bandwidth".into()))
        );
        assert_eq!(
            apply_patch(&s, &json!({"network": {"mode": "openvpn"}})),
            Err(PatchError::WrongType("network".into()))
        );
        let big = "x".repeat(MAX_PATCH_BYTES);
        assert_eq!(
            apply_patch(&s, &json!({"network": {"interface": big}})),
            Err(PatchError::TooLarge)
        );
    }

    #[test]
    fn validates_the_result() {
        let s = Settings::default();
        let err = apply_patch(&s, &json!({"bandwidth": {"peerLimit": 1}})).unwrap_err();
        assert_eq!(
            err.to_string(),
            "bandwidth.peerLimit: Choose between 5 and 500 peers."
        );
        assert!(matches!(err, PatchError::Invalid(ref e) if e.len() == 1));
    }
}
