//! Split settlement: deciding who gets what out of a single charge.
//!
//! A marketplace charge is one payment from the shopper that has to become
//! several payouts: one per shop, minus the platform's fee, plus whatever
//! third-party funders owe. This module turns a [`Quote`] into a
//! [`SettlementPlan`] and proves the result balances before any money moves.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::ids::{AccountId, ShopId};
use crate::money::{Currency, Money, Rounding, allocate_by_weights};
use crate::pricing::Quote;

/// What the platform charges for running the marketplace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlatformFeePolicy {
    /// Commission in basis points of the fee base.
    pub percentage_basis_points: i64,
    /// Flat fee per shop per order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixed_per_shop: Option<Money>,
    /// Whether commission is charged on tax as well as goods.
    ///
    /// Charging commission on tax is unusual and often not permitted, so the
    /// default excludes it.
    #[serde(default)]
    pub include_tax_in_base: bool,
    /// Rounding for the percentage component.
    #[serde(default)]
    pub rounding: Rounding,
}

impl PlatformFeePolicy {
    /// No platform fee at all.
    pub fn none() -> Self {
        Self {
            percentage_basis_points: 0,
            fixed_per_shop: None,
            include_tax_in_base: false,
            rounding: Rounding::HalfUp,
        }
    }

    /// A simple percentage commission.
    pub fn percentage(basis_points: i64) -> Self {
        Self {
            percentage_basis_points: basis_points,
            fixed_per_shop: None,
            include_tax_in_base: false,
            rounding: Rounding::HalfUp,
        }
    }

    /// Builder: add a flat per-shop component.
    pub fn plus_fixed(mut self, fixed: Money) -> Self {
        self.fixed_per_shop = Some(fixed);
        self
    }

    /// Compute the fee for one shop, never exceeding what the shop is owed.
    fn fee_for(&self, gross: Money, tax: Money) -> Result<Money> {
        let base =
            if self.include_tax_in_base { gross } else { gross.try_sub(tax)?.clamp_non_negative() };
        let mut fee = base.mul_basis_points(self.percentage_basis_points, self.rounding)?;
        if let Some(fixed) = self.fixed_per_shop {
            fee = fee.try_add(fixed)?;
        }
        fee.try_min(gross.clamp_non_negative())
    }
}

impl Default for PlatformFeePolicy {
    fn default() -> Self {
        Self::none()
    }
}

/// Where a shop's money should be sent.
#[derive(Debug, Clone, Default)]
pub struct ShopAccounts(BTreeMap<String, AccountId>);

impl ShopAccounts {
    /// An empty mapping: every shop settles to an account named after its id.
    pub fn new() -> Self {
        Self::default()
    }

    /// Map a shop onto a connected account.
    pub fn insert(&mut self, shop: ShopId, account: AccountId) -> &mut Self {
        self.0.insert(shop.to_string(), account);
        self
    }

    /// Builder form of [`Self::insert`].
    pub fn with(mut self, shop: ShopId, account: AccountId) -> Self {
        self.insert(shop, account);
        self
    }

    /// Resolve a shop's account, defaulting to the shop id itself.
    pub fn resolve(&self, shop: &ShopId) -> AccountId {
        self.0
            .get(shop.as_str())
            .cloned()
            .unwrap_or_else(|| AccountId::from_string(shop.as_str()))
    }
}

/// What one shop receives from one order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShopSettlement {
    /// The shop.
    pub shop_id: ShopId,
    /// Connected account the money goes to.
    pub account_id: AccountId,
    /// Everything the shop is owed, tax included, before the platform fee.
    pub gross: Money,
    /// Tax the shop must remit, included in `gross`.
    pub tax: Money,
    /// Platform commission taken out of `gross`.
    pub platform_fee: Money,
    /// What actually lands in the shop's account.
    pub net: Money,
    /// Part of `gross` paid by the shopper.
    pub funded_by_customer: Money,
    /// Part of `gross` paid by third-party funders.
    pub funded_by_subsidy: Money,
}

/// What one funder owes for one order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunderCharge {
    /// Account to bill.
    pub funder: AccountId,
    /// Total owed.
    pub amount: Money,
    /// Which shops the money is destined for.
    pub per_shop: BTreeMap<String, Money>,
}

/// The full money-movement plan for an order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettlementPlan {
    /// Currency of every amount.
    pub currency: Currency,
    /// What the shopper pays.
    pub collected_from_customer: Money,
    /// What funders pay.
    pub collected_from_funders: Money,
    /// Per-shop settlements.
    pub shops: Vec<ShopSettlement>,
    /// Per-funder charges.
    pub funders: Vec<FunderCharge>,
    /// Total platform commission across all shops.
    pub platform_fee_total: Money,
}

impl SettlementPlan {
    /// Build a plan from a priced quote.
    pub fn from_quote(
        quote: &Quote,
        accounts: &ShopAccounts,
        fees: &PlatformFeePolicy,
    ) -> Result<Self> {
        let currency = quote.currency;
        let mut shops = Vec::with_capacity(quote.shop_totals.len());
        let mut platform_fee_total = Money::zero(currency);

        for totals in &quote.shop_totals {
            let platform_fee = fees.fee_for(totals.merchant_gross, totals.tax)?;
            platform_fee_total = platform_fee_total.try_add(platform_fee)?;
            shops.push(ShopSettlement {
                shop_id: totals.shop_id.clone(),
                account_id: accounts.resolve(&totals.shop_id),
                gross: totals.merchant_gross,
                tax: totals.tax,
                platform_fee,
                net: totals.merchant_gross.try_sub(platform_fee)?,
                funded_by_customer: totals.customer_total,
                funded_by_subsidy: totals.subsidy_discount,
            });
        }

        let funders = quote
            .subsidies
            .iter()
            .map(|subsidy| FunderCharge {
                funder: subsidy.funder.clone(),
                amount: subsidy.amount,
                per_shop: subsidy.per_shop.clone(),
            })
            .collect();

        let plan = SettlementPlan {
            currency,
            collected_from_customer: quote.totals.total,
            collected_from_funders: quote.totals.subsidy_discount,
            shops,
            funders,
            platform_fee_total,
        };
        plan.verify()?;
        Ok(plan)
    }

    /// Settlement for one shop.
    pub fn shop(&self, shop_id: &ShopId) -> Option<&ShopSettlement> {
        self.shops.iter().find(|settlement| &settlement.shop_id == shop_id)
    }

    /// Total owed to shops before fees.
    pub fn gross_total(&self) -> Result<Money> {
        Money::sum(self.shops.iter().map(|shop| shop.gross), self.currency)
    }

    /// Total that will actually be paid out to shops.
    pub fn net_total(&self) -> Result<Money> {
        Money::sum(self.shops.iter().map(|shop| shop.net), self.currency)
    }

    /// Assert the plan balances. Called automatically by [`Self::from_quote`].
    pub fn verify(&self) -> Result<()> {
        let inflow = self.collected_from_customer.try_add(self.collected_from_funders)?;
        let outflow = self.gross_total()?;
        if inflow != outflow {
            return Err(Error::internal(format!(
                "settlement imbalance: collected {inflow} but owe {outflow}"
            )));
        }

        let customer_funded =
            Money::sum(self.shops.iter().map(|shop| shop.funded_by_customer), self.currency)?;
        if customer_funded != self.collected_from_customer {
            return Err(Error::internal(format!(
                "customer-funded shares {customer_funded} do not sum to {}",
                self.collected_from_customer
            )));
        }

        let funder_total = Money::sum(self.funders.iter().map(|f| f.amount), self.currency)?;
        if funder_total != self.collected_from_funders {
            return Err(Error::internal(format!(
                "funder charges {funder_total} do not sum to {}",
                self.collected_from_funders
            )));
        }

        for shop in &self.shops {
            if shop.platform_fee.is_negative() || shop.net.is_negative() {
                return Err(Error::internal(format!(
                    "shop {} would settle a negative amount",
                    shop.shop_id
                )));
            }
            let funded = shop.funded_by_customer.try_add(shop.funded_by_subsidy)?;
            if funded != shop.gross {
                return Err(Error::internal(format!(
                    "shop {} funding {funded} does not match gross {}",
                    shop.shop_id, shop.gross
                )));
            }
        }
        Ok(())
    }

    /// Split an amount actually collected from the shopper across the shops,
    /// proportionally to what each shop is owed by the shopper.
    ///
    /// Used when the amount charged differs from the plan (partial capture) so
    /// that every shop shares the shortfall fairly and to the cent.
    pub fn prorate_customer_amount(&self, amount: Money) -> Result<Vec<(ShopId, Money)>> {
        let weights: Vec<Money> =
            self.shops.iter().map(|shop| shop.funded_by_customer).collect();
        let shares = allocate_by_weights(amount, &weights)?;
        Ok(self
            .shops
            .iter()
            .map(|shop| shop.shop_id.clone())
            .zip(shares)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Address, CountryCode, Jurisdiction};
    use crate::cart::{Cart, CartItem};
    use crate::pricing::{
        Discount, PricingEngine, RateTableTaxCalculator, TaxRule,
    };
    use std::sync::Arc;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    async fn quote_for(items: Vec<CartItem>, discounts: Vec<Discount>) -> Quote {
        let tax = RateTableTaxCalculator::with_rules([TaxRule::new(
            "CA",
            "state",
            Jurisdiction::region(CountryCode::US, "CA"),
            1_000,
        )]);
        let engine = PricingEngine::new(Arc::new(tax));
        let mut cart = Cart::new(Currency::USD);
        cart.set_shipping_address(Address::new(CountryCode::US).with_region("CA"));
        for item in items {
            cart.add_item(item).unwrap();
        }
        engine.quote(&cart, &discounts).await.unwrap()
    }

    #[tokio::test]
    async fn single_shop_settles_the_whole_charge() {
        let quote =
            quote_for(vec![CartItem::new("shop-1", "a", "A", usd(10_000), 1).unwrap()], vec![])
                .await;
        let plan = SettlementPlan::from_quote(&quote, &ShopAccounts::new(), &PlatformFeePolicy::none())
            .unwrap();

        assert_eq!(plan.shops.len(), 1);
        assert_eq!(plan.shops[0].gross, usd(11_000));
        assert_eq!(plan.shops[0].net, usd(11_000));
        assert_eq!(plan.collected_from_customer, usd(11_000));
    }

    #[tokio::test]
    async fn platform_fee_excludes_tax_by_default() {
        let quote =
            quote_for(vec![CartItem::new("shop-1", "a", "A", usd(10_000), 1).unwrap()], vec![])
                .await;
        let plan = SettlementPlan::from_quote(
            &quote,
            &ShopAccounts::new(),
            &PlatformFeePolicy::percentage(1_000), // 10 %
        )
        .unwrap();

        // 10 % of the 100.00 of goods, not of the 110.00 gross.
        assert_eq!(plan.shops[0].platform_fee, usd(1_000));
        assert_eq!(plan.shops[0].net, usd(10_000));
        assert_eq!(plan.platform_fee_total, usd(1_000));
    }

    #[tokio::test]
    async fn fee_can_include_tax_when_configured() {
        let quote =
            quote_for(vec![CartItem::new("shop-1", "a", "A", usd(10_000), 1).unwrap()], vec![])
                .await;
        let mut policy = PlatformFeePolicy::percentage(1_000);
        policy.include_tax_in_base = true;
        let plan = SettlementPlan::from_quote(&quote, &ShopAccounts::new(), &policy).unwrap();
        assert_eq!(plan.shops[0].platform_fee, usd(1_100));
    }

    #[tokio::test]
    async fn subsidised_orders_bill_the_funder_and_keep_the_shop_whole() {
        let subsidy = Discount::amount_off("SUB", "Employer benefit", usd(3_000))
            .funded_by(AccountId::from_string("acct_employer"), "benefit");
        let quote =
            quote_for(vec![CartItem::new("shop-1", "a", "A", usd(10_000), 1).unwrap()], vec![subsidy])
                .await;
        let plan =
            SettlementPlan::from_quote(&quote, &ShopAccounts::new(), &PlatformFeePolicy::none())
                .unwrap();

        assert_eq!(plan.collected_from_customer, usd(8_000)); // 100 - 30 + 10 tax
        assert_eq!(plan.collected_from_funders, usd(3_000));
        assert_eq!(plan.shops[0].gross, usd(11_000));
        assert_eq!(plan.funders.len(), 1);
        assert_eq!(plan.funders[0].funder, AccountId::from_string("acct_employer"));
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn multi_shop_split_balances_to_the_cent() {
        let quote = quote_for(
            vec![
                CartItem::new("shop-1", "a", "A", usd(3_333), 1).unwrap(),
                CartItem::new("shop-2", "b", "B", usd(3_333), 1).unwrap(),
                CartItem::new("shop-3", "c", "C", usd(3_334), 1).unwrap(),
            ],
            vec![Discount::amount_off("TEN", "$10 off", usd(1_000))],
        )
        .await;

        let accounts = ShopAccounts::new()
            .with(ShopId::from_string("shop-1"), AccountId::from_string("acct_1"));
        let plan =
            SettlementPlan::from_quote(&quote, &accounts, &PlatformFeePolicy::percentage(250))
                .unwrap();

        assert_eq!(plan.shops.len(), 3);
        assert_eq!(plan.shop(&ShopId::from_string("shop-1")).unwrap().account_id.as_str(), "acct_1");
        // Unmapped shops fall back to an account named after the shop.
        assert_eq!(plan.shop(&ShopId::from_string("shop-2")).unwrap().account_id.as_str(), "shop-2");
        assert_eq!(plan.gross_total().unwrap(), quote.totals.merchant_gross);
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn prorating_a_partial_collection_is_exact() {
        let quote = quote_for(
            vec![
                CartItem::new("shop-1", "a", "A", usd(3_333), 1).unwrap(),
                CartItem::new("shop-2", "b", "B", usd(3_333), 1).unwrap(),
                CartItem::new("shop-3", "c", "C", usd(3_334), 1).unwrap(),
            ],
            vec![],
        )
        .await;
        let plan =
            SettlementPlan::from_quote(&quote, &ShopAccounts::new(), &PlatformFeePolicy::none())
                .unwrap();

        let shares = plan.prorate_customer_amount(usd(1_000)).unwrap();
        let total = Money::sum(shares.iter().map(|(_, amount)| *amount), Currency::USD).unwrap();
        assert_eq!(total, usd(1_000));
        assert_eq!(shares.len(), 3);
    }

    #[test]
    fn imbalanced_plans_are_rejected() {
        let plan = SettlementPlan {
            currency: Currency::USD,
            collected_from_customer: usd(1_000),
            collected_from_funders: usd(0),
            shops: vec![ShopSettlement {
                shop_id: ShopId::from_string("shop-1"),
                account_id: AccountId::from_string("acct_1"),
                gross: usd(999),
                tax: usd(0),
                platform_fee: usd(0),
                net: usd(999),
                funded_by_customer: usd(999),
                funded_by_subsidy: usd(0),
            }],
            funders: vec![],
            platform_fee_total: usd(0),
        };
        assert!(plan.verify().is_err());
    }
}
