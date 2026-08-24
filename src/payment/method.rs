//! Stored payment instruments — tokens only, never card data.
//!
//! A [`PaymentMethodRef`] is a *reference* to an instrument that lives inside
//! the gateway's vault. The only card-derived data kept here is what a shopper
//! needs to recognise the card in a list (brand, last four digits, expiry) —
//! none of which is cardholder data under PCI DSS.

use chrono::{DateTime, Datelike, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

use crate::Metadata;
use crate::address::Address;
use crate::error::{Error, Result};
use crate::gateway::GatewayId;
use crate::ids::{CustomerId, PaymentMethodId};

/// The family of instrument behind a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaymentMethodKind {
    /// A credit or debit card.
    Card(CardSummary),
    /// A bank debit / transfer scheme (SEPA, ACH, BACS…).
    BankAccount {
        /// Scheme name, e.g. `"sepa_debit"`.
        scheme: String,
        /// Last digits of the account number, for display only.
        last4: String,
        /// Account holder's country.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        country: Option<String>,
    },
    /// A wallet such as Apple Pay, Google Pay or PayPal.
    Wallet {
        /// Wallet provider identifier.
        provider: String,
    },
    /// Buy-now-pay-later.
    BuyNowPayLater {
        /// Provider identifier.
        provider: String,
    },
    /// Anything the adapter cannot classify.
    Other {
        /// Provider's own type string.
        provider_type: String,
    },
}

impl PaymentMethodKind {
    /// A short label for receipts, e.g. `"Visa •••• 4242"`.
    pub fn display_label(&self) -> String {
        match self {
            PaymentMethodKind::Card(card) => format!("{} •••• {}", card.brand, card.last4),
            PaymentMethodKind::BankAccount { scheme, last4, .. } => {
                format!("{scheme} •••• {last4}")
            }
            PaymentMethodKind::Wallet { provider } => provider.clone(),
            PaymentMethodKind::BuyNowPayLater { provider } => provider.clone(),
            PaymentMethodKind::Other { provider_type } => provider_type.clone(),
        }
    }
}

/// The non-sensitive summary of a card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardSummary {
    /// Network brand, e.g. `"visa"`.
    pub brand: String,
    /// Last four digits. Not cardholder data on its own.
    pub last4: String,
    /// Expiry month, 1-12.
    pub exp_month: u32,
    /// Four-digit expiry year.
    pub exp_year: i32,
    /// Issuing country, when the gateway reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    /// `credit`, `debit` or `prepaid`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub funding: Option<String>,
    /// Network fingerprint: lets you spot the same card added twice without
    /// ever seeing the number.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

impl CardSummary {
    /// Build a summary, rejecting values that look like real card data.
    pub fn new(brand: impl Into<String>, last4: impl Into<String>, exp_month: u32, exp_year: i32) -> Result<Self> {
        let last4 = last4.into();
        if last4.len() != 4 || !last4.chars().all(|c| c.is_ascii_digit()) {
            return Err(Error::validation(
                "last4 must be exactly four digits — never pass a full card number",
            ));
        }
        if !(1..=12).contains(&exp_month) {
            return Err(Error::validation("expiry month must be between 1 and 12"));
        }
        if !(2000..=2100).contains(&exp_year) {
            return Err(Error::validation("expiry year must be a four-digit year"));
        }
        Ok(Self {
            brand: brand.into(),
            last4,
            exp_month,
            exp_year,
            country: None,
            funding: None,
            fingerprint: None,
        })
    }

    /// Whether the card is expired at `now`.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        let year = now.year();
        let month = now.month();
        self.exp_year < year || (self.exp_year == year && self.exp_month < month)
    }
}

impl fmt::Display for CardSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} •••• {} ({:02}/{})", self.brand, self.last4, self.exp_month, self.exp_year)
    }
}

/// A tokenised instrument stored against a customer ("card on file").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaymentMethodRef {
    /// Our identifier.
    pub id: PaymentMethodId,
    /// Owning customer.
    pub customer_id: CustomerId,
    /// Gateway that holds the vaulted instrument.
    pub gateway: GatewayId,
    /// The gateway's token. This is *not* cardholder data, but it is
    /// bearer-ish, so treat it as confidential.
    pub gateway_token: String,
    /// The gateway's customer identifier, when the token is customer-scoped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_customer_id: Option<String>,
    /// What kind of instrument this is.
    pub kind: PaymentMethodKind,
    /// Billing address captured at vaulting time, used for AVS and tax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_address: Option<Address>,
    /// Whether the shopper marked this as their default instrument.
    #[serde(default)]
    pub is_default: bool,
    /// Whether the shopper agreed to future merchant-initiated charges.
    /// Required for card-on-file/recurring under card-scheme rules.
    #[serde(default)]
    pub reusable: bool,
    /// Free-form data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
    /// When the instrument was vaulted.
    pub created_at: DateTime<Utc>,
}

impl PaymentMethodRef {
    /// Create a reference to a freshly vaulted instrument.
    pub fn new(
        customer_id: CustomerId,
        gateway: GatewayId,
        gateway_token: impl Into<String>,
        kind: PaymentMethodKind,
    ) -> Self {
        Self {
            id: PaymentMethodId::new(),
            customer_id,
            gateway,
            gateway_token: gateway_token.into(),
            gateway_customer_id: None,
            kind,
            billing_address: None,
            is_default: false,
            reusable: false,
            metadata: Metadata::new(),
            created_at: Utc::now(),
        }
    }

    /// Builder: mark the instrument as usable for future off-session charges.
    pub fn reusable(mut self) -> Self {
        self.reusable = true;
        self
    }

    /// Builder: set the gateway customer id.
    pub fn with_gateway_customer(mut self, id: impl Into<String>) -> Self {
        self.gateway_customer_id = Some(id.into());
        self
    }

    /// Builder: set the billing address.
    pub fn with_billing_address(mut self, address: Address) -> Self {
        self.billing_address = Some(address);
        self
    }

    /// Label for receipts and instrument pickers.
    pub fn display_label(&self) -> String {
        self.kind.display_label()
    }

    /// Whether this instrument can be charged at `now`.
    pub fn is_usable(&self, now: DateTime<Utc>) -> bool {
        match &self.kind {
            PaymentMethodKind::Card(card) => !card.is_expired(now),
            _ => true,
        }
    }

    /// Reject an unusable instrument with a helpful message.
    pub fn ensure_usable(&self, now: DateTime<Utc>) -> Result<()> {
        if self.is_usable(now) {
            Ok(())
        } else {
            Err(Error::Declined {
                code: crate::error::DeclineCode::ExpiredCard,
                message: format!("{} has expired", self.display_label()),
            })
        }
    }
}

/// Who initiated a charge. Card schemes require this to be reported
/// correctly; off-session charges without stored consent are chargeback bait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChargeInitiator {
    /// The shopper is present and interacting with the checkout.
    #[default]
    CustomerOnSession,
    /// The shopper set this up earlier; the merchant is charging now.
    MerchantOffSession,
}

impl ChargeInitiator {
    /// Whether stored-credential consent ([`PaymentMethodRef::reusable`]) is required.
    pub fn requires_stored_consent(self) -> bool {
        matches!(self, ChargeInitiator::MerchantOffSession)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn card() -> CardSummary {
        CardSummary::new("visa", "4242", 12, 2030).unwrap()
    }

    #[test]
    fn rejects_anything_that_is_not_last_four_digits() {
        assert!(CardSummary::new("visa", "4242424242424242", 12, 2030).is_err());
        assert!(CardSummary::new("visa", "42a2", 12, 2030).is_err());
        assert!(CardSummary::new("visa", "4242", 13, 2030).is_err());
        assert!(CardSummary::new("visa", "4242", 12, 30).is_err());
    }

    #[test]
    fn expiry_check() {
        let now = Utc.with_ymd_and_hms(2031, 1, 1, 0, 0, 0).unwrap();
        assert!(card().is_expired(now));
        assert!(!card().is_expired(Utc.with_ymd_and_hms(2030, 12, 31, 0, 0, 0).unwrap()));
    }

    #[test]
    fn expired_cards_decline_before_touching_the_gateway() {
        let method = PaymentMethodRef::new(
            CustomerId::new(),
            GatewayId::from_static("mock"),
            "tok_123",
            PaymentMethodKind::Card(card()),
        );
        let now = Utc.with_ymd_and_hms(2031, 1, 1, 0, 0, 0).unwrap();
        let error = method.ensure_usable(now).unwrap_err();
        assert!(matches!(error, Error::Declined { .. }));
        assert_eq!(method.display_label(), "visa •••• 4242");
    }

    #[test]
    fn off_session_requires_consent() {
        assert!(ChargeInitiator::MerchantOffSession.requires_stored_consent());
        assert!(!ChargeInitiator::CustomerOnSession.requires_stored_consent());
    }
}
