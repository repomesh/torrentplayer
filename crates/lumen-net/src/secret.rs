// SPDX-License-Identifier: MIT OR Apache-2.0

//! A string secret that is wiped from memory when dropped and never printed.

use std::fmt;

use zeroize::Zeroize as _;

/// A password or private key held in memory.
///
/// The value is overwritten with zeros when dropped, its `Debug` output is redacted, and it does
/// not implement `Serialize`, so it cannot end up in logs or config files by accident. Persisting
/// secrets is the job of the OS keychain (see `docs/network.md`).
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The secret itself. Use only at the point where it must be sent.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether the secret is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl<'de> serde::Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_is_redacted() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        assert_eq!(s.expose(), "hunter2");
    }
}
