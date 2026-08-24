//! Shop credit: a per-customer, per-shop stored-value balance.
//!
//! Shop credit is scoped to a single shop, so it can only ever pay for that
//! shop's share of an order. The engine enforces this when it builds a tender
//! plan, which stops the classic marketplace bug where credit issued by one
//! vendor silently pays another vendor's invoice.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{CustomerId, LedgerEntryId, OrderId, ShopId};
use crate::money::{Currency, Money};

/// Why a credit balance changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CreditEntryKind {
    /// The shop granted credit (goodwill, loyalty, promotion).
    Grant {
        /// Why it was granted.
        reason: String,
    },
    /// Credit was spent on an order.
    Redemption {
        /// The order it paid for.
        order_id: OrderId,
    },
    /// A redemption was reversed, e.g. because the order was refunded.
    Reversal {
        /// The order whose redemption is being undone.
        order_id: OrderId,
    },
    /// A refund was issued as store credit instead of to the original method.
    RefundToCredit {
        /// The refunded order.
        order_id: OrderId,
    },
    /// Credit lapsed.
    Expiry,
    /// Manual correction by an operator.
    Adjustment {
        /// Audit note.
        note: String,
    },
}

/// One immutable movement on a credit account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreditEntry {
    /// Identifier.
    pub id: LedgerEntryId,
    /// Signed amount: positive adds credit, negative spends it.
    pub amount: Money,
    /// Why it happened.
    pub kind: CreditEntryKind,
    /// Balance after this entry, stored so history can be rendered cheaply.
    pub balance_after: Money,
    /// When it happened.
    pub created_at: DateTime<Utc>,
    /// Idempotency key of the operation that produced the entry, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
}

/// A customer's credit balance at one shop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShopCreditAccount {
    /// Owner of the balance.
    pub customer_id: CustomerId,
    /// Shop the credit can be spent at.
    pub shop_id: ShopId,
    /// Currency of the balance. Credit never crosses currencies.
    pub currency: Currency,
    /// Append-only history.
    pub entries: Vec<CreditEntry>,
    /// Optimistic concurrency token.
    #[serde(default)]
    pub version: u64,
}

impl ShopCreditAccount {
    /// An empty account.
    pub fn new(customer_id: CustomerId, shop_id: ShopId, currency: Currency) -> Self {
        Self { customer_id, shop_id, currency, entries: Vec::new(), version: 0 }
    }

    /// Current balance, derived from the entries.
    pub fn balance(&self) -> Money {
        self.entries
            .last()
            .map(|entry| entry.balance_after)
            .unwrap_or_else(|| Money::zero(self.currency))
    }

    /// Add credit. `amount` must be positive.
    pub fn grant(
        &mut self,
        amount: Money,
        reason: impl Into<String>,
        idempotency_key: Option<String>,
    ) -> Result<&CreditEntry> {
        if !amount.is_positive() {
            return Err(Error::validation("granted credit must be positive"));
        }
        self.append(amount, CreditEntryKind::Grant { reason: reason.into() }, idempotency_key)
    }

    /// Spend credit against an order. `amount` must be positive and available.
    pub fn redeem(
        &mut self,
        amount: Money,
        order_id: OrderId,
        idempotency_key: Option<String>,
    ) -> Result<&CreditEntry> {
        if !amount.is_positive() {
            return Err(Error::validation("redeemed credit must be positive"));
        }
        let balance = self.balance();
        balance.assert_same_currency(amount)?;
        if amount > balance {
            return Err(Error::InsufficientFunds {
                source_name: format!("shop credit at {}", self.shop_id),
                available: balance,
                required: amount,
            });
        }
        self.append(amount.negate(), CreditEntryKind::Redemption { order_id }, idempotency_key)
    }

    /// Give back credit that was spent on an order.
    pub fn reverse(
        &mut self,
        amount: Money,
        order_id: OrderId,
        idempotency_key: Option<String>,
    ) -> Result<&CreditEntry> {
        if !amount.is_positive() {
            return Err(Error::validation("reversed credit must be positive"));
        }
        let redeemed = self.redeemed_for(&order_id)?;
        if amount > redeemed {
            return Err(Error::validation(format!(
                "cannot reverse {amount} of credit: only {redeemed} was redeemed for {order_id}"
            )));
        }
        self.append(amount, CreditEntryKind::Reversal { order_id }, idempotency_key)
    }

    /// How much credit this account spent on a given order, net of reversals.
    pub fn redeemed_for(&self, order_id: &OrderId) -> Result<Money> {
        let mut total = Money::zero(self.currency);
        for entry in &self.entries {
            match &entry.kind {
                CreditEntryKind::Redemption { order_id: id } if id == order_id => {
                    total = total.try_add(entry.amount.abs())?;
                }
                CreditEntryKind::Reversal { order_id: id } if id == order_id => {
                    total = total.try_sub(entry.amount.abs())?;
                }
                _ => {}
            }
        }
        Ok(total.clamp_non_negative())
    }

    /// Whether an operation with this idempotency key was already recorded.
    pub fn has_key(&self, key: &str) -> bool {
        self.entries
            .iter()
            .any(|entry| entry.idempotency_key.as_deref() == Some(key))
    }

    fn append(
        &mut self,
        amount: Money,
        kind: CreditEntryKind,
        idempotency_key: Option<String>,
    ) -> Result<&CreditEntry> {
        amount.assert_same_currency(Money::zero(self.currency))?;
        if let Some(key) = &idempotency_key
            && self.has_key(key)
        {
            return Err(Error::conflict(
                "shop credit",
                &self.shop_id,
                format!("idempotency key '{key}' already applied"),
            ));
        }
        let balance_after = self.balance().try_add(amount)?;
        if balance_after.is_negative() {
            return Err(Error::internal("credit ledger would go negative"));
        }
        self.entries.push(CreditEntry {
            id: LedgerEntryId::new(),
            amount,
            kind,
            balance_after,
            created_at: Utc::now(),
            idempotency_key,
        });
        self.version = self.version.saturating_add(1);
        Ok(self.entries.last().expect("just pushed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn account() -> ShopCreditAccount {
        ShopCreditAccount::new(
            CustomerId::from_string("cus_1"),
            ShopId::from_string("shop-1"),
            Currency::USD,
        )
    }

    #[test]
    fn balance_is_derived_from_entries() {
        let mut account = account();
        assert_eq!(account.balance(), usd(0));
        account.grant(usd(5_000), "goodwill", None).unwrap();
        account.grant(usd(1_000), "loyalty", None).unwrap();
        assert_eq!(account.balance(), usd(6_000));
        assert_eq!(account.entries.len(), 2);
    }

    #[test]
    fn cannot_spend_more_than_the_balance() {
        let mut account = account();
        account.grant(usd(1_000), "goodwill", None).unwrap();
        let error = account.redeem(usd(1_001), OrderId::from_string("ord_1"), None).unwrap_err();
        assert!(matches!(error, Error::InsufficientFunds { .. }));
        assert_eq!(account.balance(), usd(1_000));
    }

    #[test]
    fn redemption_and_reversal_round_trip() {
        let mut account = account();
        let order = OrderId::from_string("ord_1");
        account.grant(usd(5_000), "goodwill", None).unwrap();
        account.redeem(usd(2_000), order.clone(), None).unwrap();
        assert_eq!(account.balance(), usd(3_000));
        assert_eq!(account.redeemed_for(&order).unwrap(), usd(2_000));

        account.reverse(usd(500), order.clone(), None).unwrap();
        assert_eq!(account.balance(), usd(3_500));
        assert_eq!(account.redeemed_for(&order).unwrap(), usd(1_500));

        // Over-reversing would create money out of nothing.
        assert!(account.reverse(usd(2_000), order, None).is_err());
    }

    #[test]
    fn idempotency_keys_prevent_double_application() {
        let mut account = account();
        account.grant(usd(1_000), "welcome", Some("key-1".into())).unwrap();
        let error = account.grant(usd(1_000), "welcome", Some("key-1".into())).unwrap_err();
        assert!(matches!(error, Error::Conflict { .. }));
        assert_eq!(account.balance(), usd(1_000));
    }

    #[test]
    fn currency_is_enforced() {
        let mut account = account();
        assert!(account.grant(Money::from_minor(100, Currency::EUR), "x", None).is_err());
        assert!(account.grant(usd(0), "x", None).is_err());
    }
}
