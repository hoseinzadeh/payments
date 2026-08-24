//! Discounts, promotions and third-party subsidies.
//!
//! # Merchant-funded vs. subsidised discounts
//!
//! The crate distinguishes *who pays* for a discount, because it changes both
//! the money movement and the tax base:
//!
//! * [`DiscountFunding::Merchant`] — the shop gives up revenue. The shop
//!   receives less, and in most jurisdictions the taxable base shrinks too
//!   (it is a genuine price reduction).
//! * [`DiscountFunding::Subsidy`] — a third party (the platform, a brand, an
//!   employer, a government programme) pays the difference on the shopper's
//!   behalf. The shop still receives the full price, the *funder* is billed for
//!   the subsidised portion, and the taxable base is normally **unchanged**
//!   because the shop's consideration has not changed.
//!
//! Getting this wrong is how marketplaces end up under-remitting tax, so the
//! behaviour is explicit in [`Discount::reduces_taxable_base`] and can be
//! overridden per discount when local rules differ.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::ids::{AccountId, DiscountId, LineItemId, ShopId};
use crate::money::{Money, Rounding};

/// What a discount is worth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscountValue {
    /// A percentage expressed in basis points (1 000 bp = 10 %).
    Percentage {
        /// Rate in basis points.
        basis_points: i64,
        /// Optional ceiling on the resulting amount.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_amount: Option<Money>,
    },
    /// A fixed amount off.
    Amount {
        /// The amount to subtract, capped at the remaining base.
        amount: Money,
    },
    /// Waives shipping charges for the matched scope.
    FreeShipping,
}

/// Which part of the cart a discount applies to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscountScope {
    /// Applies to every line in the cart.
    Order,
    /// Applies to all lines sold by one shop.
    Shop {
        /// The shop whose lines are discounted.
        shop_id: ShopId,
    },
    /// Applies to specific lines.
    LineItems {
        /// Lines to discount.
        line_ids: Vec<LineItemId>,
    },
    /// Applies to every line carrying one of these SKUs.
    Skus {
        /// SKUs to discount.
        skus: Vec<String>,
    },
}

/// Who absorbs the cost of a discount.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DiscountFunding {
    /// The selling shop gives up revenue.
    Merchant,
    /// A third party reimburses the shop.
    Subsidy {
        /// Account that will be billed for the subsidised amount.
        funder: AccountId,
        /// Optional label shown on statements, e.g. `"Welcome promotion"`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        program: Option<String>,
    },
}

impl DiscountFunding {
    /// The subsidising account, if any.
    pub fn funder(&self) -> Option<&AccountId> {
        match self {
            DiscountFunding::Merchant => None,
            DiscountFunding::Subsidy { funder, .. } => Some(funder),
        }
    }

    /// Whether this funding source triggers a split settlement.
    pub fn is_subsidy(&self) -> bool {
        matches!(self, DiscountFunding::Subsidy { .. })
    }
}

/// Constraints that must hold for a discount to be usable.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct DiscountConditions {
    /// Cart (or scoped) subtotal must reach this amount.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_subtotal: Option<Money>,
    /// Not usable before this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starts_at: Option<DateTime<Utc>>,
    /// Not usable at or after this instant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_at: Option<DateTime<Utc>>,
    /// Global redemption cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_redemptions: Option<u64>,
    /// Redemptions already used.
    #[serde(default)]
    pub redemptions: u64,
}

impl DiscountConditions {
    /// Check the time window and redemption cap.
    fn check(&self, now: DateTime<Utc>, scoped_subtotal: Money) -> Result<()> {
        if let Some(starts_at) = self.starts_at
            && now < starts_at
        {
            return Err(Error::validation("discount is not active yet"));
        }
        if let Some(ends_at) = self.ends_at
            && now >= ends_at
        {
            return Err(Error::validation("discount has expired"));
        }
        if let Some(max) = self.max_redemptions
            && self.redemptions >= max
        {
            return Err(Error::validation("discount redemption limit reached"));
        }
        if let Some(minimum) = self.minimum_subtotal {
            scoped_subtotal.assert_same_currency(minimum)?;
            if scoped_subtotal < minimum {
                return Err(Error::validation(format!(
                    "order does not meet the {minimum} minimum for this discount"
                )));
            }
        }
        Ok(())
    }
}

/// A promotion definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Discount {
    /// Identifier.
    pub id: DiscountId,
    /// Code the shopper types in; `None` for automatic promotions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Description shown on the receipt.
    pub description: String,
    /// What it is worth.
    pub value: DiscountValue,
    /// What it applies to.
    pub scope: DiscountScope,
    /// Who pays for it.
    pub funding: DiscountFunding,
    /// Lower numbers apply first. Stacking order is significant for
    /// percentage discounts, so it is explicit rather than implied.
    #[serde(default)]
    pub priority: i32,
    /// When `false`, this discount refuses to combine with any other.
    #[serde(default = "default_true")]
    pub stackable: bool,
    /// Eligibility rules.
    #[serde(default)]
    pub conditions: DiscountConditions,
    /// Overrides the default taxable-base behaviour of the funding source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduces_taxable_base_override: Option<bool>,
}

fn default_true() -> bool {
    true
}

impl Discount {
    /// A merchant-funded percentage discount over the whole order.
    pub fn percentage_off(
        code: impl Into<String>,
        description: impl Into<String>,
        basis_points: i64,
    ) -> Self {
        Self {
            id: DiscountId::new(),
            code: Some(code.into()),
            description: description.into(),
            value: DiscountValue::Percentage { basis_points, max_amount: None },
            scope: DiscountScope::Order,
            funding: DiscountFunding::Merchant,
            priority: 0,
            stackable: true,
            conditions: DiscountConditions::default(),
            reduces_taxable_base_override: None,
        }
    }

    /// A merchant-funded fixed-amount discount over the whole order.
    pub fn amount_off(
        code: impl Into<String>,
        description: impl Into<String>,
        amount: Money,
    ) -> Self {
        Self {
            id: DiscountId::new(),
            code: Some(code.into()),
            description: description.into(),
            value: DiscountValue::Amount { amount },
            scope: DiscountScope::Order,
            funding: DiscountFunding::Merchant,
            priority: 0,
            stackable: true,
            conditions: DiscountConditions::default(),
            reduces_taxable_base_override: None,
        }
    }

    /// Builder: restrict the discount to a scope.
    pub fn with_scope(mut self, scope: DiscountScope) -> Self {
        self.scope = scope;
        self
    }

    /// Builder: make a third party fund the discount, which turns it into a
    /// payment split at settlement time.
    pub fn funded_by(mut self, funder: AccountId, program: impl Into<String>) -> Self {
        self.funding = DiscountFunding::Subsidy {
            funder,
            program: Some(program.into()),
        };
        self
    }

    /// Builder: set the stacking priority (lower applies first).
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }

    /// Builder: forbid combining this discount with others.
    pub fn exclusive(mut self) -> Self {
        self.stackable = false;
        self
    }

    /// Builder: set eligibility conditions.
    pub fn with_conditions(mut self, conditions: DiscountConditions) -> Self {
        self.conditions = conditions;
        self
    }

    /// Builder: explicitly control whether the discount lowers the tax base.
    pub fn reducing_taxable_base(mut self, reduces: bool) -> Self {
        self.reduces_taxable_base_override = Some(reduces);
        self
    }

    /// Whether this discount lowers the amount tax is computed on.
    ///
    /// Defaults to `true` for merchant-funded discounts and `false` for
    /// subsidies, which is the standard treatment in the US and the EU.
    pub fn reduces_taxable_base(&self) -> bool {
        self.reduces_taxable_base_override.unwrap_or(!self.funding.is_subsidy())
    }

    /// Whether the discount matches a given line.
    pub fn matches_line(&self, shop_id: &ShopId, line_id: &LineItemId, sku: &str) -> bool {
        match &self.scope {
            DiscountScope::Order => true,
            DiscountScope::Shop { shop_id: scoped } => scoped == shop_id,
            DiscountScope::LineItems { line_ids } => line_ids.contains(line_id),
            DiscountScope::Skus { skus } => skus.iter().any(|candidate| candidate == sku),
        }
    }

    /// Validate eligibility against the subtotal of the lines it matches.
    pub fn check_eligibility(&self, now: DateTime<Utc>, scoped_subtotal: Money) -> Result<()> {
        self.conditions.check(now, scoped_subtotal)
    }

    /// Compute the raw discount amount against a base, before capping.
    pub fn amount_for(&self, base: Money, rounding: Rounding) -> Result<Money> {
        let raw = match &self.value {
            DiscountValue::Percentage { basis_points, max_amount } => {
                let computed = base.mul_basis_points(*basis_points, rounding)?;
                match max_amount {
                    Some(cap) => computed.try_min(*cap)?,
                    None => computed,
                }
            }
            DiscountValue::Amount { amount } => *amount,
            // Shipping waivers are handled by the pricing engine, which knows
            // the shipping charges; against item bases they are worth nothing.
            DiscountValue::FreeShipping => Money::zero(base.currency()),
        };
        // Never discount more than what is left, and never produce a credit.
        raw.try_min(base)?.try_max(Money::zero(base.currency()))
    }

    /// Whether the discount waives shipping.
    pub fn is_free_shipping(&self) -> bool {
        matches!(self.value, DiscountValue::FreeShipping)
    }
}

/// Sorts discounts into the order they must be applied and rejects illegal
/// combinations (a non-stackable discount alongside anything else).
pub fn order_for_application(discounts: &[Discount]) -> Result<Vec<&Discount>> {
    let mut ordered: Vec<&Discount> = discounts.iter().collect();
    ordered.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            // Subsidies last: they must not shrink the base that merchant
            // discounts are computed from, and their allocation depends on the
            // final merchant-discounted price.
            .then(a.funding.is_subsidy().cmp(&b.funding.is_subsidy()))
            .then(a.id.cmp(&b.id))
    });

    if ordered.len() > 1
        && let Some(exclusive) = ordered.iter().find(|discount| !discount.stackable)
    {
        return Err(Error::validation(format!(
            "discount '{}' cannot be combined with other promotions",
            exclusive.code.clone().unwrap_or_else(|| exclusive.description.clone())
        )));
    }
    Ok(ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;
    use chrono::Duration;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    #[test]
    fn funding_drives_the_taxable_base_by_default() {
        let merchant = Discount::percentage_off("SAVE10", "10% off", 1_000);
        assert!(merchant.reduces_taxable_base());

        let subsidised = Discount::amount_off("WELCOME", "Platform credit", usd(500))
            .funded_by(AccountId::from_string("acct_platform"), "welcome");
        assert!(!subsidised.reduces_taxable_base());
        assert!(subsidised.funding.is_subsidy());

        let overridden = subsidised.clone().reducing_taxable_base(true);
        assert!(overridden.reduces_taxable_base());
    }

    #[test]
    fn amounts_are_capped_at_the_base_and_never_negative() {
        let discount = Discount::amount_off("BIG", "$50 off", usd(5_000));
        assert_eq!(discount.amount_for(usd(2_000), Rounding::HalfUp).unwrap(), usd(2_000));
        assert_eq!(discount.amount_for(usd(0), Rounding::HalfUp).unwrap(), usd(0));
    }

    #[test]
    fn percentage_respects_its_cap() {
        let mut discount = Discount::percentage_off("HALF", "50% off", 5_000);
        discount.value =
            DiscountValue::Percentage { basis_points: 5_000, max_amount: Some(usd(1_000)) };
        assert_eq!(discount.amount_for(usd(10_000), Rounding::HalfUp).unwrap(), usd(1_000));
        assert_eq!(discount.amount_for(usd(1_000), Rounding::HalfUp).unwrap(), usd(500));
    }

    #[test]
    fn conditions_gate_eligibility() {
        let now = Utc::now();
        let discount = Discount::amount_off("MIN", "$5 off orders over $50", usd(500))
            .with_conditions(DiscountConditions {
                minimum_subtotal: Some(usd(5_000)),
                ends_at: Some(now - Duration::hours(1)),
                ..Default::default()
            });
        assert!(discount.check_eligibility(now, usd(6_000)).is_err(), "expired");

        let active = Discount::amount_off("MIN", "$5 off orders over $50", usd(500))
            .with_conditions(DiscountConditions {
                minimum_subtotal: Some(usd(5_000)),
                ..Default::default()
            });
        assert!(active.check_eligibility(now, usd(4_999)).is_err(), "below minimum");
        assert!(active.check_eligibility(now, usd(5_000)).is_ok());
    }

    #[test]
    fn redemption_cap_is_enforced() {
        let discount = Discount::percentage_off("ONCE", "one use", 1_000).with_conditions(
            DiscountConditions { max_redemptions: Some(1), redemptions: 1, ..Default::default() },
        );
        assert!(discount.check_eligibility(Utc::now(), usd(10_000)).is_err());
    }

    #[test]
    fn scope_matching() {
        let shop = ShopId::from_string("shop-1");
        let line = LineItemId::from_string("li-1");
        let shop_scoped = Discount::percentage_off("S", "shop", 500)
            .with_scope(DiscountScope::Shop { shop_id: shop.clone() });
        assert!(shop_scoped.matches_line(&shop, &line, "tee"));
        assert!(!shop_scoped.matches_line(&ShopId::from_string("shop-2"), &line, "tee"));

        let sku_scoped = Discount::percentage_off("K", "sku", 500)
            .with_scope(DiscountScope::Skus { skus: vec!["tee".into()] });
        assert!(sku_scoped.matches_line(&shop, &line, "tee"));
        assert!(!sku_scoped.matches_line(&shop, &line, "mug"));
    }

    #[test]
    fn ordering_puts_subsidies_last_and_rejects_exclusives() {
        let a = Discount::percentage_off("A", "a", 1_000).with_priority(10);
        let b = Discount::percentage_off("B", "b", 500).with_priority(1);
        let c = Discount::amount_off("C", "c", usd(100))
            .funded_by(AccountId::from_string("acct_x"), "p")
            .with_priority(1);

        let candidates = [a.clone(), b.clone(), c.clone()];
        let ordered = order_for_application(&candidates).unwrap();
        assert_eq!(ordered[0].code.as_deref(), Some("B"));
        assert_eq!(ordered[1].code.as_deref(), Some("C"));
        assert_eq!(ordered[2].code.as_deref(), Some("A"));

        let exclusive = Discount::percentage_off("X", "x", 2_000).exclusive();
        assert!(order_for_application(&[a, exclusive]).is_err());
    }
}
