// SPDX-License-Identifier: MIT OR Apache-2.0

//! Secrets (proxy passwords, WireGuard keys) live in the OS credential store, never in
//! `settings.json`.
//!
//! Values are held as [`SecretString`], which zeroizes on drop and redacts itself from `Debug`.
//! Nothing here formats a value into a log line or an error message. The frontend can set or
//! clear a secret and ask whether one is set, but it can never read one back.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use secrecy::zeroize::Zeroize;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Service name for TorrentPlayer's entries in the OS credential store.
pub const KEYRING_SERVICE: &str = "io.torrentplayer.desktop";

/// Service name used before the rename to TorrentPlayer. Entries still filed under it move to
/// [`KEYRING_SERVICE`] the first time they are read.
pub const LEGACY_KEYRING_SERVICE: &str = "io.github.lumen-app.lumen";

/// RFC 1929 limits SOCKS5 passwords to 255 bytes.
const MAX_PASSWORD_BYTES: usize = 255;

/// A secret TorrentPlayer can store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub enum SecretKey {
    /// The SOCKS5 proxy password (`network.socks5`).
    Socks5Password,
    /// The WireGuard `[Interface] PrivateKey`.
    WireGuardPrivateKey,
    /// The WireGuard `[Peer] PresharedKey`, if the configuration has one.
    WireGuardPresharedKey,
}

impl SecretKey {
    /// Every key.
    pub const ALL: [Self; 3] = [
        Self::Socks5Password,
        Self::WireGuardPrivateKey,
        Self::WireGuardPresharedKey,
    ];

    /// The account name of this secret's credential store entry.
    #[must_use]
    pub fn account(self) -> &'static str {
        match self {
            Self::Socks5Password => "network.socks5.password",
            Self::WireGuardPrivateKey => "network.wireguard.private-key",
            Self::WireGuardPresharedKey => "network.wireguard.preshared-key",
        }
    }

    /// A user-facing name.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Socks5Password => "Proxy password",
            Self::WireGuardPrivateKey => "WireGuard private key",
            Self::WireGuardPresharedKey => "WireGuard preshared key",
        }
    }

    /// Checks and normalizes a value before it is stored. WireGuard keys are trimmed; passwords
    /// are kept exactly as typed.
    ///
    /// # Errors
    ///
    /// Returns [`SecretError::Invalid`] with a message that never contains the value.
    pub fn prepare(self, value: &str) -> Result<SecretString, SecretError> {
        match self {
            Self::Socks5Password => {
                if value.is_empty() {
                    return Err(SecretError::Invalid("Enter a password."));
                }
                if value.len() > MAX_PASSWORD_BYTES || value.contains('\0') {
                    return Err(SecretError::Invalid(
                        "Proxy passwords are at most 255 bytes, without NUL characters.",
                    ));
                }
                Ok(SecretString::from(value))
            }
            Self::WireGuardPrivateKey | Self::WireGuardPresharedKey => {
                let key = value.trim();
                if is_wireguard_key(key) {
                    Ok(SecretString::from(key))
                } else {
                    Err(SecretError::Invalid(
                        "WireGuard keys are 44 base64 characters ending in \"=\".",
                    ))
                }
            }
        }
    }
}

/// A 32-byte key in standard base64: 43 alphabet characters and one `=`.
fn is_wireguard_key(key: &str) -> bool {
    key.len() == 44
        && key.ends_with('=')
        && key[..43]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// Why a secret could not be stored or read. Messages never contain secret values or paths.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretError {
    /// The value was rejected before reaching the store.
    #[error("{0}")]
    Invalid(&'static str),
    /// There is no usable credential store on this system.
    #[error(
        "No system credential store is available, so secrets cannot be saved. On Linux, install \
         and unlock a Secret Service provider such as GNOME Keyring or KWallet."
    )]
    Unavailable,
    /// The credential store refused the request (for example, it is locked).
    #[error("The system credential store refused the request. Unlock it and try again.")]
    Refused,
}

/// Where secrets are kept. [`KeyringStore`] in the app; [`MemoryStore`] in tests.
pub trait SecretStore: Send + Sync + fmt::Debug {
    /// Whether the store can be used at all.
    fn available(&self) -> bool;

    /// Stores (or replaces) a secret.
    ///
    /// # Errors
    ///
    /// Returns an error if the store is unavailable or refuses the write.
    fn set(&self, key: SecretKey, value: &SecretString) -> Result<(), SecretError>;

    /// Reads a secret, if one is set. For Rust subsystems only; never sent to the frontend.
    ///
    /// # Errors
    ///
    /// Returns an error if the store is unavailable or refuses the read.
    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError>;

    /// Removes a secret. Removing one that is not set succeeds.
    ///
    /// # Errors
    ///
    /// Returns an error if the store is unavailable or refuses the delete.
    fn clear(&self, key: SecretKey) -> Result<(), SecretError>;

    /// Whether a secret is set.
    ///
    /// # Errors
    ///
    /// Returns an error if the store is unavailable or refuses the read.
    fn is_set(&self, key: SecretKey) -> Result<bool, SecretError> {
        Ok(self.get(key)?.is_some())
    }
}

/// Which secrets are set, without their values.
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent flags, one per secret"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export)]
pub struct SecretStatus {
    /// Whether the SOCKS5 proxy password is set.
    pub socks5_password: bool,
    /// Whether the WireGuard private key is set.
    pub wire_guard_private_key: bool,
    /// Whether the WireGuard preshared key is set.
    pub wire_guard_preshared_key: bool,
    /// Whether the OS credential store can be used.
    pub store_available: bool,
}

impl SecretStatus {
    /// Asks `store` which secrets are set. Read errors count as "not set" and are logged.
    #[must_use]
    pub fn of(store: &dyn SecretStore) -> Self {
        let available = store.available();
        let is_set = |key: SecretKey| {
            available
                && store.is_set(key).unwrap_or_else(|error| {
                    tracing::warn!(key = key.account(), %error, "could not check a secret");
                    false
                })
        };
        Self {
            socks5_password: is_set(SecretKey::Socks5Password),
            wire_guard_private_key: is_set(SecretKey::WireGuardPrivateKey),
            wire_guard_preshared_key: is_set(SecretKey::WireGuardPresharedKey),
            store_available: available,
        }
    }
}

/// The OS credential store: Windows Credential Manager, macOS Keychain, or the Secret Service
/// (GNOME Keyring, `KWallet`) on Linux. Calls block, so run them off the async runtime.
#[derive(Debug, Clone)]
pub struct KeyringStore {
    service: String,
    legacy: Option<String>,
}

impl KeyringStore {
    /// A store whose entries are filed under `service` (normally [`KEYRING_SERVICE`]).
    #[must_use]
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            legacy: None,
        }
    }

    /// Also reads entries still filed under `service` (normally [`LEGACY_KEYRING_SERVICE`]),
    /// moving each one to the current service when it is found.
    #[must_use]
    pub fn with_legacy(mut self, service: impl Into<String>) -> Self {
        self.legacy = Some(service.into());
        self
    }

    fn entry(&self, key: SecretKey) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(&self.service, key.account()).map_err(|e| map_keyring(key, &e))
    }

    fn legacy_entry(&self, key: SecretKey) -> Option<keyring::Entry> {
        keyring::Entry::new(self.legacy.as_deref()?, key.account()).ok()
    }

    /// Moves a legacy entry to the current service. Returns its value, if there was one.
    fn take_legacy(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        let Some(legacy) = self.legacy_entry(key) else {
            return Ok(None);
        };
        let secret = match legacy.get_password() {
            Ok(mut value) => {
                let secret = SecretString::from(value.as_str());
                value.zeroize();
                secret
            }
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => return Err(map_keyring(key, &e)),
        };
        // Removed only once the copy is stored, so a failure loses nothing.
        if self.set(key, &secret).is_ok() {
            let _ = legacy.delete_credential();
        }
        Ok(Some(secret))
    }
}

impl SecretStore for KeyringStore {
    fn available(&self) -> bool {
        keyring::Entry::store_status().is_ok()
    }

    fn set(&self, key: SecretKey, value: &SecretString) -> Result<(), SecretError> {
        self.entry(key)?
            .set_password(value.expose_secret())
            .map_err(|e| map_keyring(key, &e))
    }

    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        match self.entry(key)?.get_password() {
            Ok(mut value) => {
                let secret = SecretString::from(value.as_str());
                value.zeroize();
                Ok(Some(secret))
            }
            Err(keyring::Error::NoEntry) => self.take_legacy(key),
            Err(e) => Err(map_keyring(key, &e)),
        }
    }

    fn clear(&self, key: SecretKey) -> Result<(), SecretError> {
        // A cleared secret must not come back from the legacy service.
        if let Some(legacy) = self.legacy_entry(key) {
            let _ = legacy.delete_credential();
        }
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(map_keyring(key, &e)),
        }
    }
}

/// Maps a `keyring` error and logs only its kind: some variants carry the raw secret bytes
/// (`BadEncoding`, `BadDataFormat`), so the error itself is never formatted.
fn map_keyring(key: SecretKey, error: &keyring::Error) -> SecretError {
    let (kind, mapped) = match error {
        keyring::Error::NoDefaultStore => ("no-default-store", SecretError::Unavailable),
        keyring::Error::NoStorageAccess(_) => ("no-storage-access", SecretError::Refused),
        keyring::Error::PlatformFailure(_) => ("platform-failure", SecretError::Refused),
        keyring::Error::TooLong(..) => (
            "too-long",
            SecretError::Invalid("The value is too long for the credential store."),
        ),
        keyring::Error::BadEncoding(_) | keyring::Error::BadDataFormat(..) => {
            ("bad-encoding", SecretError::Refused)
        }
        _ => ("other", SecretError::Refused),
    };
    tracing::warn!(key = key.account(), kind, "credential store error");
    mapped
}

/// Keeps secrets in memory only, for tests. TorrentPlayer never falls back to it.
#[derive(Debug, Default)]
pub struct MemoryStore {
    values: Mutex<HashMap<SecretKey, SecretString>>,
    unavailable: bool,
}

impl MemoryStore {
    /// An empty, available store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that behaves like a system without a credential store.
    #[must_use]
    pub fn unavailable() -> Self {
        Self {
            unavailable: true,
            ..Self::default()
        }
    }

    fn check(&self) -> Result<(), SecretError> {
        if self.unavailable {
            Err(SecretError::Unavailable)
        } else {
            Ok(())
        }
    }
}

impl SecretStore for MemoryStore {
    fn available(&self) -> bool {
        !self.unavailable
    }

    fn set(&self, key: SecretKey, value: &SecretString) -> Result<(), SecretError> {
        self.check()?;
        let copy = SecretString::from(value.expose_secret());
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, copy);
        Ok(())
    }

    fn get(&self, key: SecretKey) -> Result<Option<SecretString>, SecretError> {
        self.check()?;
        Ok(self
            .values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
            .map(|v| SecretString::from(v.expose_secret())))
    }

    fn clear(&self, key: SecretKey) -> Result<(), SecretError> {
        self.check()?;
        self.values
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=";

    #[test]
    fn values_are_validated_without_echoing_them() {
        let err = SecretKey::WireGuardPrivateKey
            .prepare("not-a-key-hunter2")
            .unwrap_err();
        assert!(!err.to_string().contains("hunter2"));
        assert!(SecretKey::Socks5Password.prepare("").is_err());
        assert!(SecretKey::Socks5Password.prepare(&"x".repeat(256)).is_err());
        assert!(SecretKey::Socks5Password.prepare("a\0b").is_err());
        let padded = format!("  {KEY}\n");
        let key = SecretKey::WireGuardPrivateKey.prepare(&padded).unwrap();
        assert_eq!(key.expose_secret(), KEY, "keys are trimmed");
        let password = SecretKey::Socks5Password.prepare(" pass word ").unwrap();
        assert_eq!(password.expose_secret(), " pass word ", "passwords are not");
    }

    #[test]
    fn debug_never_shows_secret_values() {
        let store = MemoryStore::new();
        let value = SecretKey::Socks5Password.prepare("hunter2").unwrap();
        assert!(!format!("{value:?}").contains("hunter2"));
        store.set(SecretKey::Socks5Password, &value).unwrap();
        assert!(!format!("{store:?}").contains("hunter2"));
        let read = store.get(SecretKey::Socks5Password).unwrap().unwrap();
        assert!(!format!("{read:?}").contains("hunter2"));
        assert_eq!(read.expose_secret(), "hunter2");
    }

    #[test]
    fn status_reports_only_presence() {
        let store = MemoryStore::new();
        assert_eq!(
            SecretStatus::of(&store),
            SecretStatus {
                store_available: true,
                ..SecretStatus::default()
            }
        );
        let key = SecretKey::WireGuardPrivateKey.prepare(KEY).unwrap();
        store.set(SecretKey::WireGuardPrivateKey, &key).unwrap();
        let status = SecretStatus::of(&store);
        assert!(status.wire_guard_private_key && !status.socks5_password);
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains(KEY));
        store.clear(SecretKey::WireGuardPrivateKey).unwrap();
        store.clear(SecretKey::WireGuardPrivateKey).unwrap();
        assert!(!SecretStatus::of(&store).wire_guard_private_key);
    }

    #[test]
    fn unavailable_store_fails_closed() {
        let store = MemoryStore::unavailable();
        let value = SecretKey::Socks5Password.prepare("pw").unwrap();
        assert_eq!(
            store.set(SecretKey::Socks5Password, &value),
            Err(SecretError::Unavailable)
        );
        assert_eq!(SecretStatus::of(&store), SecretStatus::default());
    }

    #[test]
    fn account_names_are_distinct() {
        let mut names: Vec<_> = SecretKey::ALL.iter().map(|k| k.account()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SecretKey::ALL.len());
    }
}
