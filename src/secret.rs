//! Handling of sensitive values.
//!
//! # PCI DSS posture
//!
//! This crate is designed so that a Primary Account Number (PAN), CVV/CVC or
//! magnetic-stripe data **can never enter the process**. There is deliberately
//! no type in the public API that accepts a card number: instruments are only
//! ever referenced by a gateway-issued token (see
//! [`PaymentMethodRef`](crate::payment::method::PaymentMethodRef)). Collecting
//! card data must happen in the gateway's own hosted fields / SDK, which keeps
//! the integrating service in PCI DSS scope **SAQ A**.
//!
//! Values that *are* sensitive but must be held in memory — API keys, webhook
//! signing secrets, single-use client secrets — are wrapped in [`SecretString`],
//! which:
//!
//! * redacts itself in `Debug` and `Display`, so it cannot leak through logs,
//!   `tracing` fields, panic messages or `serde` serialisation;
//! * overwrites its buffer on drop, shortening the window in which the secret
//!   is recoverable from freed memory;
//! * compares in constant time, so it cannot be recovered by timing a
//!   comparison loop.

use serde::{Deserialize, Deserializer};
use std::fmt;
use subtle::ConstantTimeEq;

/// A string that will not be printed, logged or serialised.
///
/// ```
/// use payments::secret::SecretString;
///
/// let key = SecretString::new("sk_live_do_not_log_me");
/// assert_eq!(format!("{key:?}"), "SecretString(***redacted***)");
/// assert_eq!(key.expose(), "sk_live_do_not_log_me");
/// assert!(key.constant_time_eq("sk_live_do_not_log_me"));
/// ```
#[derive(Clone, Default)]
pub struct SecretString(String);

impl SecretString {
    /// Wrap a value as a secret.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Read a secret from an environment variable.
    pub fn from_env(key: &str) -> Option<Self> {
        std::env::var(key).ok().map(Self::new)
    }

    /// Deliberately verbose accessor: every call site that unwraps a secret is
    /// easy to audit by grepping for `expose`.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// The number of bytes in the secret (safe to log).
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Compare against a candidate without leaking length-independent timing.
    pub fn constant_time_eq(&self, candidate: &str) -> bool {
        self.0.as_bytes().ct_eq(candidate.as_bytes()).into()
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(***redacted***)")
    }
}

impl fmt::Display for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***redacted***")
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        self.constant_time_eq(&other.0)
    }
}

impl Eq for SecretString {}

impl Drop for SecretString {
    fn drop(&mut self) {
        // Overwrite the heap buffer in place. `write_volatile` prevents the
        // optimiser from removing the stores as dead writes.
        let bytes = unsafe { self.0.as_bytes_mut() };
        for byte in bytes.iter_mut() {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl From<String> for SecretString {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for SecretString {
    fn from(value: &str) -> Self {
        Self(value.to_owned())
    }
}

/// Secrets can be read from configuration but never written back out.
impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(SecretString::new)
    }
}

/// Mask all but the last `visible` characters of an identifier.
///
/// Use this for values that are *not* secret but are still personal, such as a
/// gift card code shown in an audit log.
pub fn mask_tail(value: &str, visible: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= visible {
        return "*".repeat(chars.len());
    }
    let hidden = chars.len() - visible;
    let mut out = "*".repeat(hidden);
    out.extend(&chars[hidden..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_never_render_their_contents() {
        let secret = SecretString::new("whsec_topsecret");
        assert_eq!(format!("{secret}"), "***redacted***");
        assert_eq!(format!("{secret:?}"), "SecretString(***redacted***)");
        assert!(!format!("{secret:?}").contains("topsecret"));
        assert_eq!(secret.len(), 15);
    }

    #[test]
    fn comparison_is_value_based() {
        assert_eq!(SecretString::new("a"), SecretString::new("a"));
        assert_ne!(SecretString::new("a"), SecretString::new("b"));
        assert!(!SecretString::new("abc").constant_time_eq("abcd"));
    }

    #[test]
    fn masking() {
        assert_eq!(mask_tail("GIFT-1234-5678", 4), "**********5678");
        assert_eq!(mask_tail("abc", 4), "***");
    }
}
