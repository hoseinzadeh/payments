//! The crate-wide error type.
//!
//! Errors carry enough structure for a caller to decide *programmatically*
//! whether an operation may be retried ([`Error::is_retryable`]) and whether the
//! failure was caused by the caller or by an upstream system
//! ([`Error::category`]). Gateway-specific failures are normalised into
//! [`DeclineCode`] so that business logic never has to match on Stripe's
//! `card_declined` and PayPal's `INSTRUMENT_DECLINED` separately.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::money::{Currency, Money};

/// Convenient result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Coarse classification used for metrics, alerting and retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// The request was malformed or violated a business rule.
    Validation,
    /// The referenced entity does not exist.
    NotFound,
    /// The operation conflicts with the current state of the entity.
    Conflict,
    /// The payment instrument or issuer refused the charge.
    Declined,
    /// The caller is not allowed to perform the operation.
    Authorization,
    /// A downstream dependency failed (gateway, storage, network).
    Upstream,
    /// A defect in configuration, e.g. no gateway registered for a currency.
    Configuration,
    /// An internal invariant was violated.
    Internal,
}

/// Gateway-agnostic decline reasons.
///
/// Adapters map their provider's codes onto this enum so that callers can
/// implement one retry/messaging strategy for all gateways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclineCode {
    /// Generic refusal with no further detail from the issuer.
    GenericDecline,
    /// Not enough funds on the instrument.
    InsufficientFunds,
    /// Card expired.
    ExpiredCard,
    /// Wrong card number.
    IncorrectNumber,
    /// Wrong CVC/CVV.
    IncorrectCvc,
    /// Wrong postal code / AVS failure.
    IncorrectPostalCode,
    /// The issuer flagged the transaction as fraudulent.
    Fraudulent,
    /// The card is reported lost or stolen.
    LostOrStolenCard,
    /// Velocity or amount limit exceeded.
    LimitExceeded,
    /// Strong customer authentication (3-D Secure) is required.
    AuthenticationRequired,
    /// The issuer or gateway is temporarily unavailable; safe to retry.
    ProcessingError,
    /// The instrument does not support this currency or capability.
    Unsupported,
}

impl DeclineCode {
    /// Whether retrying the *same* instrument could plausibly succeed later.
    pub fn is_retryable(self) -> bool {
        matches!(self, DeclineCode::ProcessingError | DeclineCode::LimitExceeded)
    }

    /// A safe, non-leaking message suitable for showing to a shopper.
    pub fn customer_message(self) -> &'static str {
        match self {
            DeclineCode::InsufficientFunds => "Your card has insufficient funds.",
            DeclineCode::ExpiredCard => "Your card has expired.",
            DeclineCode::IncorrectNumber => "The card number is incorrect.",
            DeclineCode::IncorrectCvc => "The security code is incorrect.",
            DeclineCode::IncorrectPostalCode => "The postal code does not match your card.",
            DeclineCode::AuthenticationRequired => {
                "Your bank requires additional authentication to complete this payment."
            }
            DeclineCode::ProcessingError => {
                "We could not process your payment right now. Please try again."
            }
            DeclineCode::Unsupported => "This payment method cannot be used for this purchase.",
            // Never tell the shopper the card was flagged as fraudulent or stolen.
            DeclineCode::GenericDecline
            | DeclineCode::Fraudulent
            | DeclineCode::LostOrStolenCard
            | DeclineCode::LimitExceeded => {
                "Your card was declined. Please use a different payment method."
            }
        }
    }
}

impl fmt::Display for DeclineCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            DeclineCode::GenericDecline => "generic_decline",
            DeclineCode::InsufficientFunds => "insufficient_funds",
            DeclineCode::ExpiredCard => "expired_card",
            DeclineCode::IncorrectNumber => "incorrect_number",
            DeclineCode::IncorrectCvc => "incorrect_cvc",
            DeclineCode::IncorrectPostalCode => "incorrect_postal_code",
            DeclineCode::Fraudulent => "fraudulent",
            DeclineCode::LostOrStolenCard => "lost_or_stolen_card",
            DeclineCode::LimitExceeded => "limit_exceeded",
            DeclineCode::AuthenticationRequired => "authentication_required",
            DeclineCode::ProcessingError => "processing_error",
            DeclineCode::Unsupported => "unsupported",
        };
        f.write_str(s)
    }
}

/// The error type returned by every fallible operation in this crate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A value failed validation before any side effect happened.
    #[error("validation error: {0}")]
    Validation(String),

    /// Arithmetic or parsing problem in the money layer.
    #[error("money error: {0}")]
    Money(String),

    /// A split or proration could not be computed.
    #[error("allocation error: {0}")]
    Allocation(String),

    /// Two amounts in different currencies were combined.
    #[error("currency mismatch: expected {expected}, got {actual}")]
    CurrencyMismatch {
        /// The currency that was expected.
        expected: Currency,
        /// The currency that was supplied.
        actual: Currency,
    },

    /// A currency code is not recognised.
    #[error("unknown currency: {0}")]
    UnknownCurrency(String),

    /// The requested entity does not exist.
    #[error("{kind} not found: {id}")]
    NotFound {
        /// Entity type, e.g. `"cart"`.
        kind: &'static str,
        /// Identifier that was looked up.
        id: String,
    },

    /// The entity exists but is in a state that forbids the operation.
    #[error("invalid state transition for {kind} {id}: {from} -> {to}")]
    InvalidTransition {
        /// Entity type, e.g. `"order"`.
        kind: &'static str,
        /// Identifier of the entity.
        id: String,
        /// Current state.
        from: String,
        /// Attempted state.
        to: String,
    },

    /// A concurrent write was detected (optimistic locking).
    #[error("conflict on {kind} {id}: {message}")]
    Conflict {
        /// Entity type.
        kind: &'static str,
        /// Identifier of the entity.
        id: String,
        /// Human-readable detail.
        message: String,
    },

    /// The same idempotency key was reused with a different payload.
    #[error("idempotency key '{key}' was reused with a different request payload")]
    IdempotencyConflict {
        /// The offending key.
        key: String,
    },

    /// A tender (gift card, shop credit, gateway charge) could not cover the amount.
    #[error("insufficient funds: {source_name} has {available} but {required} is required")]
    InsufficientFunds {
        /// Which balance ran out.
        source_name: String,
        /// Amount available.
        available: Money,
        /// Amount needed.
        required: Money,
    },

    /// The payment instrument was declined.
    #[error("payment declined ({code}): {message}")]
    Declined {
        /// Normalised decline reason.
        code: DeclineCode,
        /// Provider-supplied detail (already redacted by the adapter).
        message: String,
    },

    /// The gateway returned an error.
    #[error("gateway '{gateway}' error{}: {message}", .provider_code.as_ref().map(|c| format!(" [{c}]")).unwrap_or_default())]
    Gateway {
        /// Gateway identifier, e.g. `"stripe"`.
        gateway: String,
        /// Raw provider error code, when available.
        provider_code: Option<String>,
        /// Provider message.
        message: String,
        /// Whether the caller may safely retry with the same idempotency key.
        retryable: bool,
    },

    /// A webhook signature did not verify.
    #[error("webhook signature verification failed: {0}")]
    WebhookVerification(String),

    /// The storage backend failed.
    #[error("storage error: {0}")]
    Storage(String),

    /// Requested capability is not supported by the selected gateway.
    #[error("gateway '{gateway}' does not support {capability}")]
    UnsupportedCapability {
        /// Gateway identifier.
        gateway: String,
        /// Capability name, e.g. `"partial_capture"`.
        capability: &'static str,
    },

    /// The engine is misconfigured.
    #[error("configuration error: {0}")]
    Configuration(String),

    /// An internal invariant was violated. This is always a bug in the crate.
    #[error("internal error: {0}")]
    Internal(String),
}

impl Error {
    /// Shorthand for [`Error::Validation`].
    pub fn validation(message: impl Into<String>) -> Self {
        Error::Validation(message.into())
    }

    /// Shorthand for [`Error::Money`].
    pub fn money(message: impl Into<String>) -> Self {
        Error::Money(message.into())
    }

    /// Shorthand for [`Error::Allocation`].
    pub fn allocation(message: impl Into<String>) -> Self {
        Error::Allocation(message.into())
    }

    /// Shorthand for [`Error::Storage`].
    pub fn storage(message: impl Into<String>) -> Self {
        Error::Storage(message.into())
    }

    /// Shorthand for [`Error::Internal`].
    pub fn internal(message: impl Into<String>) -> Self {
        Error::Internal(message.into())
    }

    /// Shorthand for [`Error::Configuration`].
    pub fn configuration(message: impl Into<String>) -> Self {
        Error::Configuration(message.into())
    }

    /// Shorthand for [`Error::NotFound`].
    pub fn not_found(kind: &'static str, id: impl fmt::Display) -> Self {
        Error::NotFound { kind, id: id.to_string() }
    }

    /// Shorthand for [`Error::Conflict`].
    pub fn conflict(kind: &'static str, id: impl fmt::Display, message: impl Into<String>) -> Self {
        Error::Conflict { kind, id: id.to_string(), message: message.into() }
    }

    /// Classify the error for metrics and control flow.
    pub fn category(&self) -> ErrorCategory {
        match self {
            Error::Validation(_)
            | Error::Money(_)
            | Error::Allocation(_)
            | Error::CurrencyMismatch { .. }
            | Error::UnknownCurrency(_)
            | Error::InsufficientFunds { .. } => ErrorCategory::Validation,
            Error::NotFound { .. } => ErrorCategory::NotFound,
            Error::InvalidTransition { .. }
            | Error::Conflict { .. }
            | Error::IdempotencyConflict { .. } => ErrorCategory::Conflict,
            Error::Declined { .. } => ErrorCategory::Declined,
            Error::WebhookVerification(_) => ErrorCategory::Authorization,
            Error::Gateway { .. } | Error::Storage(_) => ErrorCategory::Upstream,
            Error::UnsupportedCapability { .. } | Error::Configuration(_) => {
                ErrorCategory::Configuration
            }
            Error::Internal(_) => ErrorCategory::Internal,
        }
    }

    /// Whether retrying the operation could succeed without changing the input.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Gateway { retryable, .. } => *retryable,
            Error::Declined { code, .. } => code.is_retryable(),
            Error::Storage(_) => true,
            _ => false,
        }
    }

    /// A message that is safe to display to an end user.
    pub fn customer_message(&self) -> String {
        match self {
            Error::Declined { code, .. } => code.customer_message().to_owned(),
            Error::InsufficientFunds { .. } => {
                "The selected payment methods do not cover the order total.".to_owned()
            }
            Error::Validation(message) => message.clone(),
            Error::Gateway { .. } | Error::Storage(_) | Error::Internal(_) => {
                "Something went wrong while processing your payment. Please try again.".to_owned()
            }
            other => other.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories_and_retryability() {
        let declined =
            Error::Declined { code: DeclineCode::ProcessingError, message: "try again".into() };
        assert_eq!(declined.category(), ErrorCategory::Declined);
        assert!(declined.is_retryable());

        let fraud = Error::Declined { code: DeclineCode::Fraudulent, message: "nope".into() };
        assert!(!fraud.is_retryable());
        // Never leak the fraud signal to the shopper.
        assert!(!fraud.customer_message().to_lowercase().contains("fraud"));

        assert_eq!(Error::not_found("cart", "abc").category(), ErrorCategory::NotFound);
    }
}
