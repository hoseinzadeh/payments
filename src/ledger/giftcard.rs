//! Gift cards.
//!
//! The redeemable code is a bearer instrument: anyone holding it can spend the
//! balance. It is therefore **never stored in the clear** — only a SHA-256 hash
//! is persisted, exactly as you would treat a password. Lookups hash the
//! candidate code and compare hashes, and the display form is masked.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};
use crate::ids::{GiftCardId, OrderId, ShopId};
use crate::money::{Currency, Money};
use crate::secret::mask_tail;

/// Hash a gift card code for storage and lookup.
///
/// Codes are upper-cased and stripped of separators first so that
/// `abcd-efgh` and `ABCDEFGH` resolve to the same card.
pub fn hash_gift_card_code(code: &str) -> String {
    let normalised: String =
        code.chars().filter(|c| c.is_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    let digest = Sha256::digest(normalised.as_bytes());
    hex::encode(digest)
}

/// Lifecycle of a gift card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GiftCardStatus {
    /// Usable.
    #[default]
    Active,
    /// Fully spent.
    Depleted,
    /// Past its expiry date.
    Expired,
    /// Blocked by an operator (fraud, chargeback on the purchase).
    Voided,
}

/// A stored-value card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GiftCard {
    /// Identifier.
    pub id: GiftCardId,
    /// SHA-256 of the normalised code. The code itself is never stored.
    pub code_hash: String,
    /// Masked form for display, e.g. `"********5678"`.
    pub masked_code: String,
    /// Currency of the balance.
    pub currency: Currency,
    /// Amount the card was issued with.
    pub initial_balance: Money,
    /// Amount still available.
    pub balance: Money,
    /// When set, the card is only valid at this shop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shop_id: Option<ShopId>,
    /// Current state.
    #[serde(default)]
    pub status: GiftCardStatus,
    /// Expiry, if the card has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Redemptions, for audit and reversal.
    #[serde(default)]
    pub redemptions: Vec<GiftCardRedemption>,
    /// Optimistic concurrency token.
    #[serde(default)]
    pub version: u64,
    /// Issue time.
    pub created_at: DateTime<Utc>,
}

/// One movement against a gift card.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GiftCardRedemption {
    /// Order the card paid for.
    pub order_id: OrderId,
    /// Signed amount: negative spends, positive restores.
    pub amount: Money,
    /// When it happened.
    pub occurred_at: DateTime<Utc>,
    /// Idempotency key of the operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

impl GiftCard {
    /// Issue a card for `code` with `amount` on it.
    ///
    /// The plaintext `code` is consumed here and never retained.
    pub fn issue(code: &str, amount: Money) -> Result<Self> {
        if !amount.is_positive() {
            return Err(Error::validation("gift card value must be positive"));
        }
        if code.chars().filter(|c| c.is_alphanumeric()).count() < 8 {
            return Err(Error::validation("gift card codes must have at least 8 characters"));
        }
        Ok(Self {
            id: GiftCardId::new(),
            code_hash: hash_gift_card_code(code),
            masked_code: mask_tail(code, 4),
            currency: amount.currency(),
            initial_balance: amount,
            balance: amount,
            shop_id: None,
            status: GiftCardStatus::Active,
            expires_at: None,
            redemptions: Vec::new(),
            version: 0,
            created_at: Utc::now(),
        })
    }

    /// Builder: restrict the card to one shop.
    pub fn for_shop(mut self, shop_id: ShopId) -> Self {
        self.shop_id = Some(shop_id);
        self
    }

    /// Builder: set an expiry.
    pub fn expiring_at(mut self, at: DateTime<Utc>) -> Self {
        self.expires_at = Some(at);
        self
    }

    /// Whether `code` matches this card, compared in constant time.
    pub fn matches_code(&self, code: &str) -> bool {
        let candidate = hash_gift_card_code(code);
        candidate.as_bytes().ct_eq(self.code_hash.as_bytes()).into()
    }

    /// Whether the card can be spent at `now`.
    pub fn is_redeemable(&self, now: DateTime<Utc>) -> bool {
        if self.status != GiftCardStatus::Active || !self.balance.is_positive() {
            return false;
        }
        match self.expires_at {
            Some(expiry) => now < expiry,
            None => true,
        }
    }

    /// Reject an unusable card with a precise reason.
    pub fn ensure_redeemable(&self, now: DateTime<Utc>) -> Result<()> {
        match self.status {
            GiftCardStatus::Voided => {
                return Err(Error::validation("this gift card is no longer valid"));
            }
            GiftCardStatus::Expired => {
                return Err(Error::validation("this gift card has expired"));
            }
            GiftCardStatus::Depleted => {
                return Err(Error::validation("this gift card has no balance left"));
            }
            GiftCardStatus::Active => {}
        }
        if let Some(expiry) = self.expires_at
            && now >= expiry
        {
            return Err(Error::validation("this gift card has expired"));
        }
        if !self.balance.is_positive() {
            return Err(Error::validation("this gift card has no balance left"));
        }
        Ok(())
    }

    /// The most that can be applied to an order, capped by the balance.
    pub fn applicable_amount(&self, wanted: Money, now: DateTime<Utc>) -> Result<Money> {
        if !self.is_redeemable(now) {
            return Ok(Money::zero(self.currency));
        }
        self.balance.assert_same_currency(wanted)?;
        wanted.try_min(self.balance)
    }

    /// Spend `amount` against `order`.
    pub fn redeem(
        &mut self,
        amount: Money,
        order_id: OrderId,
        now: DateTime<Utc>,
        idempotency_key: Option<String>,
    ) -> Result<()> {
        self.ensure_redeemable(now)?;
        if !amount.is_positive() {
            return Err(Error::validation("redeemed amount must be positive"));
        }
        self.balance.assert_same_currency(amount)?;
        if amount > self.balance {
            return Err(Error::InsufficientFunds {
                source_name: format!("gift card {}", self.masked_code),
                available: self.balance,
                required: amount,
            });
        }
        if let Some(key) = &idempotency_key
            && self.redemptions.iter().any(|r| r.idempotency_key.as_deref() == Some(key))
        {
            return Err(Error::conflict(
                "gift card",
                &self.id,
                format!("idempotency key '{key}' already applied"),
            ));
        }

        self.balance = self.balance.try_sub(amount)?;
        self.redemptions.push(GiftCardRedemption {
            order_id,
            amount: amount.negate(),
            occurred_at: now,
            idempotency_key,
        });
        if self.balance.is_zero() {
            self.status = GiftCardStatus::Depleted;
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Put money back on the card after a refund.
    pub fn restore(&mut self, amount: Money, order_id: OrderId, now: DateTime<Utc>) -> Result<()> {
        if !amount.is_positive() {
            return Err(Error::validation("restored amount must be positive"));
        }
        let spent = self.redeemed_for(&order_id)?;
        if amount > spent {
            return Err(Error::validation(format!(
                "cannot restore {amount}: only {spent} was redeemed for {order_id}"
            )));
        }
        self.balance = self.balance.try_add(amount)?;
        self.redemptions.push(GiftCardRedemption {
            order_id,
            amount,
            occurred_at: now,
            idempotency_key: None,
        });
        if self.status == GiftCardStatus::Depleted && self.balance.is_positive() {
            self.status = GiftCardStatus::Active;
        }
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Net amount spent on `order_id`.
    pub fn redeemed_for(&self, order_id: &OrderId) -> Result<Money> {
        let mut total = Money::zero(self.currency);
        for redemption in self.redemptions.iter().filter(|r| &r.order_id == order_id) {
            total = total.try_sub(redemption.amount)?;
        }
        Ok(total.clamp_non_negative())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn card() -> GiftCard {
        GiftCard::issue("GIFT-1234-5678", usd(5_000)).unwrap()
    }

    #[test]
    fn codes_are_hashed_normalised_and_masked() {
        let card = card();
        assert_ne!(card.code_hash, "GIFT-1234-5678");
        assert_eq!(card.code_hash.len(), 64);
        assert!(!card.masked_code.contains("1234"));
        assert!(card.matches_code("gift12345678"));
        assert!(card.matches_code("GIFT-1234-5678"));
        assert!(!card.matches_code("GIFT-1234-5679"));
        assert!(GiftCard::issue("short", usd(100)).is_err());
    }

    #[test]
    fn redemption_reduces_the_balance_and_depletes_the_card() {
        let mut card = card();
        let order = OrderId::from_string("ord_1");
        let now = Utc::now();
        card.redeem(usd(2_000), order.clone(), now, None).unwrap();
        assert_eq!(card.balance, usd(3_000));
        assert_eq!(card.status, GiftCardStatus::Active);

        card.redeem(usd(3_000), order.clone(), now, None).unwrap();
        assert_eq!(card.status, GiftCardStatus::Depleted);
        assert!(card.ensure_redeemable(now).is_err());
        assert_eq!(card.redeemed_for(&order).unwrap(), usd(5_000));
    }

    #[test]
    fn cannot_overspend() {
        let mut card = card();
        let error = card
            .redeem(usd(5_001), OrderId::from_string("ord_1"), Utc::now(), None)
            .unwrap_err();
        assert!(matches!(error, Error::InsufficientFunds { .. }));
    }

    #[test]
    fn expiry_blocks_redemption() {
        let now = Utc::now();
        let mut expired = card().expiring_at(now - Duration::days(1));
        assert!(!expired.is_redeemable(now));
        assert!(expired.redeem(usd(100), OrderId::from_string("o"), now, None).is_err());
        assert_eq!(expired.applicable_amount(usd(100), now).unwrap(), usd(0));
    }

    #[test]
    fn refunds_restore_the_balance() {
        let mut card = card();
        let order = OrderId::from_string("ord_1");
        let now = Utc::now();
        card.redeem(usd(5_000), order.clone(), now, None).unwrap();
        assert_eq!(card.status, GiftCardStatus::Depleted);

        card.restore(usd(1_000), order.clone(), now).unwrap();
        assert_eq!(card.balance, usd(1_000));
        assert_eq!(card.status, GiftCardStatus::Active);
        assert_eq!(card.redeemed_for(&order).unwrap(), usd(4_000));

        assert!(card.restore(usd(9_999), order, now).is_err());
    }

    #[test]
    fn applicable_amount_is_capped_by_the_balance() {
        let card = card();
        let now = Utc::now();
        assert_eq!(card.applicable_amount(usd(9_000), now).unwrap(), usd(5_000));
        assert_eq!(card.applicable_amount(usd(1_000), now).unwrap(), usd(1_000));
    }

    #[test]
    fn idempotent_redemption_is_rejected_not_duplicated() {
        let mut card = card();
        let order = OrderId::from_string("ord_1");
        let now = Utc::now();
        card.redeem(usd(1_000), order.clone(), now, Some("k1".into())).unwrap();
        assert!(card.redeem(usd(1_000), order, now, Some("k1".into())).is_err());
        assert_eq!(card.balance, usd(4_000));
    }
}
