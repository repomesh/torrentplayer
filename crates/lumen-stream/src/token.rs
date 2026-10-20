// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-session access token for the loopback server.

use std::fmt;

use subtle::ConstantTimeEq;

/// Number of random bytes in a token (256 bits).
const TOKEN_BYTES: usize = 32;
const HEX: &[u8; 16] = b"0123456789abcdef";

/// A random secret that every request to the loopback server must present.
///
/// It stops other local programs, and websites that try to reach `127.0.0.1`, from reading
/// streams. The `Debug` implementation is redacted so the token never reaches logs.
#[derive(Clone)]
pub struct SessionToken(String);

impl SessionToken {
    /// Generates a new token from the operating system's secure random number generator.
    ///
    /// # Errors
    ///
    /// Returns an error if the OS random number generator is unavailable.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes)?;
        let mut hex = String::with_capacity(TOKEN_BYTES * 2);
        for b in bytes {
            hex.push(char::from(HEX[usize::from(b >> 4)]));
            hex.push(char::from(HEX[usize::from(b & 0x0f)]));
        }
        Ok(Self(hex))
    }

    /// The token as it appears in URLs.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Compares a presented token against this one in constant time.
    #[must_use]
    pub fn matches(&self, presented: &[u8]) -> bool {
        // Length is not secret (it is fixed), so an early length check leaks nothing.
        presented.len() == self.0.len() && bool::from(self.0.as_bytes().ct_eq(presented))
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex_of_fixed_length() {
        let a = SessionToken::generate().unwrap();
        let b = SessionToken::generate().unwrap();
        assert_eq!(a.as_str().len(), TOKEN_BYTES * 2);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.as_str(), b.as_str());
    }

    #[test]
    fn matching() {
        let t = SessionToken::generate().unwrap();
        assert!(t.matches(t.as_str().as_bytes()));
        assert!(!t.matches(b""));
        assert!(!t.matches(&t.as_str().as_bytes()[1..]));
        let mut wrong = t.as_str().as_bytes().to_vec();
        wrong[0] = if wrong[0] == b'0' { b'1' } else { b'0' };
        assert!(!t.matches(&wrong));
    }

    #[test]
    fn debug_is_redacted() {
        let t = SessionToken::generate().unwrap();
        assert!(!format!("{t:?}").contains(t.as_str()));
    }
}
