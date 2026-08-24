//! The pricing engine: turns a [`Cart`] into a fully-costed [`Quote`].
//!
//! The engine is pure and side-effect free apart from the (async) call into the
//! [`TaxCalculator`], which makes it cheap enough to run on every keystroke for
//! the live totals a checkout page needs.
//!
//! # The money identity
//!
//! Every quote satisfies, for each line and in aggregate:
//!
//! ```text
//! merchant_gross = customer_total + subsidy_total
//! ```
//!
//! In words: what a shop is owed equals what the shopper pays plus what
//! third-party funders pay on the shopper's behalf. [`Quote::verify`] asserts
//! this and is called automatically at the end of [`PricingEngine::quote`], so a
//! rounding regression can never silently reach settlement.

pub mod discount;
pub mod fx;
pub mod tax;

pub use discount::{
    Discount, DiscountConditions, DiscountFunding, DiscountScope, DiscountValue,
    order_for_application,
};
pub use fx::{CurrencyConverter, ExchangeRate, StaticCurrencyConverter};
pub use tax::{
    NoTaxCalculator, RateTableTaxCalculator, TaxCalculator, TaxCode, TaxComponent, TaxLineRequest,
    TaxLineResult, TaxMode, TaxQuote, TaxRequest, TaxRule,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::address::{CountryCode, Jurisdiction};
use crate::cart::Cart;
use crate::error::{Error, Result};
use crate::ids::{AccountId, DiscountId, FulfillmentGroupId, LineItemId, ShopId};
use crate::money::{Currency, Money, Rounding, allocate_by_weights};

/// A discount as it landed on one specific line or shipment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedDiscount {
    /// The promotion that produced this amount.
    pub discount_id: DiscountId,
    /// Promotion code, if it had one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// Receipt description.
    pub description: String,
    /// Who pays for it.
    pub funding: DiscountFunding,
    /// The amount taken off here.
    pub amount: Money,
    /// Whether this amount lowered the taxable base.
    pub reduced_taxable_base: bool,
}

impl AppliedDiscount {
    /// The subsidising account, if this discount is third-party funded.
    pub fn funder(&self) -> Option<&AccountId> {
        self.funding.funder()
    }
}

/// A fully priced cart line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricedLine {
    /// The cart line this came from.
    pub line_id: LineItemId,
    /// Selling shop.
    pub shop_id: ShopId,
    /// Merchant SKU.
    pub sku: String,
    /// Display name.
    pub name: String,
    /// Units purchased.
    pub quantity: u32,
    /// Price per unit.
    pub unit_price: Money,
    /// `unit_price * quantity`.
    pub subtotal: Money,
    /// Discounts that landed on this line.
    pub discounts: Vec<AppliedDiscount>,
    /// Portion of the discounts funded by the shop itself.
    pub merchant_discount: Money,
    /// Portion of the discounts funded by third parties.
    pub subsidy_discount: Money,
    /// Amount tax was computed on.
    pub taxable_base: Money,
    /// Tax for this line.
    pub tax: Money,
    /// Per-jurisdiction tax breakdown.
    pub tax_components: Vec<TaxComponent>,
    /// What the shopper pays for this line.
    pub customer_total: Money,
    /// What the shop is owed for this line, before platform fees.
    pub merchant_gross: Money,
    /// The fulfilment group this line ships in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulfillment_group_id: Option<FulfillmentGroupId>,
}

/// A fully priced shipment charge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PricedShipping {
    /// The shipment.
    pub group_id: FulfillmentGroupId,
    /// Shop responsible for it.
    pub shop_id: ShopId,
    /// List price of the shipment.
    pub price: Money,
    /// Discounts applied (e.g. free-shipping promotions).
    pub discounts: Vec<AppliedDiscount>,
    /// Shop-funded portion of the discounts.
    pub merchant_discount: Money,
    /// Third-party-funded portion of the discounts.
    pub subsidy_discount: Money,
    /// Amount tax was computed on.
    pub taxable_base: Money,
    /// Tax on the shipping charge.
    pub tax: Money,
    /// What the shopper pays for shipping.
    pub customer_total: Money,
    /// What the shop is owed for shipping.
    pub merchant_gross: Money,
}

/// Aggregate figures for a whole quote.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuoteTotals {
    /// Sum of all line subtotals before any discount.
    pub subtotal: Money,
    /// Sum of all shipping list prices.
    pub shipping: Money,
    /// Discounts funded by shops.
    pub merchant_discount: Money,
    /// Discounts funded by third parties.
    pub subsidy_discount: Money,
    /// Total tax.
    pub tax: Money,
    /// What the shopper owes: this is the amount to collect with tenders.
    pub total: Money,
    /// What all shops are owed in aggregate, before platform fees.
    pub merchant_gross: Money,
}

/// Per-shop aggregation, the basis for split settlement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShopTotals {
    /// The shop.
    pub shop_id: ShopId,
    /// Items subtotal before discounts.
    pub subtotal: Money,
    /// Shipping list price.
    pub shipping: Money,
    /// Shop-funded discounts.
    pub merchant_discount: Money,
    /// Third-party-funded discounts on this shop's items.
    pub subsidy_discount: Money,
    /// Tax collected on this shop's supply.
    pub tax: Money,
    /// Shopper-paid amount attributable to this shop.
    pub customer_total: Money,
    /// Amount owed to the shop before platform fees.
    pub merchant_gross: Money,
}

/// How much one funder owes, and for what.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubsidyTotals {
    /// Account that will be billed.
    pub funder: AccountId,
    /// Total subsidised, across all shops.
    pub amount: Money,
    /// Breakdown per shop, so each shop can be reimbursed correctly.
    pub per_shop: BTreeMap<String, Money>,
}

/// A complete, immutable pricing result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quote {
    /// Currency of every amount in the quote.
    pub currency: Currency,
    /// Tax treatment used.
    pub tax_mode: TaxMode,
    /// Priced lines, in cart order.
    pub lines: Vec<PricedLine>,
    /// Priced shipments.
    pub shipping: Vec<PricedShipping>,
    /// Aggregates.
    pub totals: QuoteTotals,
    /// Per-shop aggregates.
    pub shop_totals: Vec<ShopTotals>,
    /// Per-funder aggregates.
    pub subsidies: Vec<SubsidyTotals>,
    /// Promotion codes that were rejected, with the reason, so the UI can
    /// explain why a code the shopper typed did nothing.
    #[serde(default)]
    pub rejected_discounts: Vec<RejectedDiscount>,
    /// When this quote was produced.
    pub priced_at: DateTime<Utc>,
}

/// A promotion that could not be applied.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RejectedDiscount {
    /// The code the shopper entered, or the promotion description.
    pub code: String,
    /// Why it was rejected, safe to show to the shopper.
    pub reason: String,
}

impl Quote {
    /// The amount the shopper must pay.
    pub fn amount_due(&self) -> Money {
        self.totals.total
    }

    /// Totals for one shop.
    pub fn shop(&self, shop_id: &ShopId) -> Option<&ShopTotals> {
        self.shop_totals.iter().find(|totals| &totals.shop_id == shop_id)
    }

    /// Check the core money identity. Called automatically after pricing.
    pub fn verify(&self) -> Result<()> {
        let expected = self.totals.total.try_add(self.totals.subsidy_discount)?;
        if expected != self.totals.merchant_gross {
            return Err(Error::internal(format!(
                "quote imbalance: customer {} + subsidy {} != merchant gross {}",
                self.totals.total, self.totals.subsidy_discount, self.totals.merchant_gross
            )));
        }

        let shop_customer =
            Money::sum(self.shop_totals.iter().map(|s| s.customer_total), self.currency)?;
        if shop_customer != self.totals.total {
            return Err(Error::internal(format!(
                "shop customer totals {shop_customer} do not sum to order total {}",
                self.totals.total
            )));
        }

        let subsidy = Money::sum(self.subsidies.iter().map(|s| s.amount), self.currency)?;
        if subsidy != self.totals.subsidy_discount {
            return Err(Error::internal(format!(
                "subsidy totals {subsidy} do not sum to {}",
                self.totals.subsidy_discount
            )));
        }
        if self.totals.total.is_negative() {
            return Err(Error::internal("quote total is negative"));
        }
        Ok(())
    }
}

/// Configuration for [`PricingEngine`].
#[derive(Debug, Clone)]
pub struct PricingConfig {
    /// Whether catalogue prices include tax.
    pub tax_mode: TaxMode,
    /// Rounding used for percentages and prorations.
    pub rounding: Rounding,
    /// Jurisdiction used when the cart has no address yet, so that a checkout
    /// page can still show plausible live totals.
    pub fallback_jurisdiction: Jurisdiction,
}

impl Default for PricingConfig {
    fn default() -> Self {
        Self {
            tax_mode: TaxMode::Exclusive,
            rounding: Rounding::HalfUp,
            fallback_jurisdiction: Jurisdiction::country(CountryCode::US),
        }
    }
}

/// Prices carts.
#[derive(Clone)]
pub struct PricingEngine {
    tax: Arc<dyn TaxCalculator>,
    config: PricingConfig,
}

impl PricingEngine {
    /// Build an engine with default configuration.
    pub fn new(tax: Arc<dyn TaxCalculator>) -> Self {
        Self { tax, config: PricingConfig::default() }
    }

    /// Build an engine with explicit configuration.
    pub fn with_config(tax: Arc<dyn TaxCalculator>, config: PricingConfig) -> Self {
        Self { tax, config }
    }

    /// The configuration in use.
    pub fn config(&self) -> &PricingConfig {
        &self.config
    }

    /// Price `cart` with `discounts` applied.
    ///
    /// Discounts that fail their eligibility conditions are skipped and
    /// reported in [`Quote::rejected_discounts`] rather than failing the whole
    /// quote — a shopper mistyping a promo code must not break checkout.
    pub async fn quote(&self, cart: &Cart, discounts: &[Discount]) -> Result<Quote> {
        cart.validate()?;
        let currency = cart.currency;
        let now = Utc::now();
        let rounding = self.config.rounding;

        let mut state = PricingState::new(cart)?;
        let mut rejected = Vec::new();

        let ordered = match order_for_application(discounts) {
            Ok(ordered) => ordered,
            Err(error) => {
                // A conflicting stack is reported, and we price without any
                // discount rather than refusing to render a checkout page.
                rejected.push(RejectedDiscount {
                    code: "*".to_owned(),
                    reason: error.customer_message(),
                });
                Vec::new()
            }
        };

        for discount in ordered {
            if let Err(error) = state.apply(discount, rounding, now, currency) {
                rejected.push(RejectedDiscount {
                    code: discount
                        .code
                        .clone()
                        .unwrap_or_else(|| discount.description.clone()),
                    reason: error.customer_message(),
                });
            }
        }

        let destination = cart
            .shipping_address
            .as_ref()
            .or(cart.billing_address.as_ref())
            .map(|address| address.jurisdiction())
            .unwrap_or_else(|| self.config.fallback_jurisdiction.clone());

        let tax_quote = self.tax.quote(&state.tax_request(currency, self.config.tax_mode, destination)?).await?;
        state.finish(&tax_quote, currency, self.config.tax_mode, now, rejected)
    }
}

/// Mutable working set used while discounts are applied.
struct PricingState {
    lines: Vec<LineState>,
    shipping: Vec<ShippingState>,
}

struct LineState {
    line_id: LineItemId,
    shop_id: ShopId,
    sku: String,
    name: String,
    quantity: u32,
    unit_price: Money,
    subtotal: Money,
    remaining: Money,
    discounts: Vec<AppliedDiscount>,
    fulfillment_group_id: Option<FulfillmentGroupId>,
    tax_code: TaxCode,
}

struct ShippingState {
    group_id: FulfillmentGroupId,
    shop_id: ShopId,
    price: Money,
    remaining: Money,
    discounts: Vec<AppliedDiscount>,
    tax_code: TaxCode,
    items: Vec<LineItemId>,
}

impl PricingState {
    fn new(cart: &Cart) -> Result<Self> {
        let lines = cart
            .items
            .iter()
            .map(|item| {
                let subtotal = item.subtotal()?;
                Ok(LineState {
                    line_id: item.id.clone(),
                    shop_id: item.shop_id.clone(),
                    sku: item.sku.clone(),
                    name: item.name.clone(),
                    quantity: item.quantity,
                    unit_price: item.unit_price,
                    subtotal,
                    remaining: subtotal,
                    discounts: Vec::new(),
                    fulfillment_group_id: item.fulfillment_group_id.clone(),
                    tax_code: item.tax_code.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let shipping = cart
            .fulfillment_groups
            .iter()
            .map(|group| ShippingState {
                group_id: group.id.clone(),
                shop_id: group.shop_id.clone(),
                price: group.shipping_price,
                remaining: group.shipping_price,
                discounts: Vec::new(),
                tax_code: group.shipping_tax_code.clone(),
                items: group.items.clone(),
            })
            .collect();

        Ok(Self { lines, shipping })
    }

    fn apply(
        &mut self,
        discount: &Discount,
        rounding: Rounding,
        now: DateTime<Utc>,
        currency: Currency,
    ) -> Result<()> {
        if discount.is_free_shipping() {
            return self.apply_free_shipping(discount, now, currency);
        }

        let matched: Vec<usize> = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| discount.matches_line(&line.shop_id, &line.line_id, &line.sku))
            .map(|(index, _)| index)
            .collect();

        if matched.is_empty() {
            return Err(Error::validation("this promotion does not apply to anything in your cart"));
        }

        let scoped_subtotal =
            Money::sum(matched.iter().map(|index| self.lines[*index].subtotal), currency)?;
        discount.check_eligibility(now, scoped_subtotal)?;

        let scoped_remaining =
            Money::sum(matched.iter().map(|index| self.lines[*index].remaining), currency)?;
        if scoped_remaining.is_zero() {
            return Err(Error::validation("nothing left to discount on these items"));
        }

        let total = discount.amount_for(scoped_remaining, rounding)?;
        if total.is_zero() {
            return Err(Error::validation("this promotion is worth nothing on this cart"));
        }

        let weights: Vec<Money> =
            matched.iter().map(|index| self.lines[*index].remaining).collect();
        let shares = allocate_by_weights(total, &weights)?;

        for (index, amount) in matched.into_iter().zip(shares) {
            if amount.is_zero() {
                continue;
            }
            let line = &mut self.lines[index];
            line.remaining = line.remaining.try_sub(amount)?;
            line.discounts.push(AppliedDiscount {
                discount_id: discount.id.clone(),
                code: discount.code.clone(),
                description: discount.description.clone(),
                funding: discount.funding.clone(),
                amount,
                reduced_taxable_base: discount.reduces_taxable_base(),
            });
        }
        Ok(())
    }

    fn apply_free_shipping(
        &mut self,
        discount: &Discount,
        now: DateTime<Utc>,
        currency: Currency,
    ) -> Result<()> {
        let subtotal = Money::sum(self.lines.iter().map(|line| line.subtotal), currency)?;
        discount.check_eligibility(now, subtotal)?;

        let matched: Vec<usize> = self
            .shipping
            .iter()
            .enumerate()
            .filter(|(_, group)| self.shipping_matches(discount, group))
            .map(|(index, _)| index)
            .collect();

        let mut waived = false;
        for index in matched {
            let group = &mut self.shipping[index];
            if group.remaining.is_zero() {
                continue;
            }
            let amount = group.remaining;
            group.remaining = Money::zero(currency);
            group.discounts.push(AppliedDiscount {
                discount_id: discount.id.clone(),
                code: discount.code.clone(),
                description: discount.description.clone(),
                funding: discount.funding.clone(),
                amount,
                reduced_taxable_base: discount.reduces_taxable_base(),
            });
            waived = true;
        }

        if waived {
            Ok(())
        } else {
            Err(Error::validation("there is no shipping charge to waive"))
        }
    }

    fn shipping_matches(&self, discount: &Discount, group: &ShippingState) -> bool {
        match &discount.scope {
            DiscountScope::Order => true,
            DiscountScope::Shop { shop_id } => shop_id == &group.shop_id,
            DiscountScope::LineItems { line_ids } => {
                group.items.iter().any(|item| line_ids.contains(item))
            }
            DiscountScope::Skus { skus } => group.items.iter().any(|item| {
                self.lines
                    .iter()
                    .any(|line| &line.line_id == item && skus.contains(&line.sku))
            }),
        }
    }

    fn tax_request(
        &self,
        currency: Currency,
        mode: TaxMode,
        destination: Jurisdiction,
    ) -> Result<TaxRequest> {
        let mut lines = Vec::with_capacity(self.lines.len() + self.shipping.len());
        for line in &self.lines {
            lines.push(TaxLineRequest {
                reference: line.line_id.to_string(),
                tax_code: line.tax_code.clone(),
                taxable_base: taxable_base(line.subtotal, &line.discounts)?,
            });
        }
        for group in &self.shipping {
            lines.push(TaxLineRequest {
                reference: group.group_id.to_string(),
                tax_code: group.tax_code.clone(),
                taxable_base: taxable_base(group.price, &group.discounts)?,
            });
        }
        Ok(TaxRequest {
            currency,
            mode,
            destination,
            origin: None,
            customer_tax_id: None,
            lines,
        })
    }

    fn finish(
        self,
        tax_quote: &TaxQuote,
        currency: Currency,
        tax_mode: TaxMode,
        priced_at: DateTime<Utc>,
        rejected_discounts: Vec<RejectedDiscount>,
    ) -> Result<Quote> {
        let zero = Money::zero(currency);
        let mut priced_lines = Vec::with_capacity(self.lines.len());

        for line in &self.lines {
            let (merchant_discount, subsidy_discount) = split_funding(&line.discounts, currency)?;
            let taxable = taxable_base(line.subtotal, &line.discounts)?;
            let result = tax_quote
                .line(line.line_id.as_str())
                .ok_or_else(|| Error::internal(format!("tax result missing for {}", line.line_id)))?;

            let net_of_discounts =
                line.subtotal.try_sub(merchant_discount)?.try_sub(subsidy_discount)?;
            let (customer_total, merchant_gross) = match tax_mode {
                TaxMode::Exclusive => (
                    net_of_discounts.try_add(result.tax)?,
                    line.subtotal.try_sub(merchant_discount)?.try_add(result.tax)?,
                ),
                TaxMode::Inclusive => {
                    (net_of_discounts, line.subtotal.try_sub(merchant_discount)?)
                }
            };

            priced_lines.push(PricedLine {
                line_id: line.line_id.clone(),
                shop_id: line.shop_id.clone(),
                sku: line.sku.clone(),
                name: line.name.clone(),
                quantity: line.quantity,
                unit_price: line.unit_price,
                subtotal: line.subtotal,
                discounts: line.discounts.clone(),
                merchant_discount,
                subsidy_discount,
                taxable_base: taxable,
                tax: result.tax,
                tax_components: result.components.clone(),
                customer_total,
                merchant_gross,
                fulfillment_group_id: line.fulfillment_group_id.clone(),
            });
        }

        let mut priced_shipping = Vec::with_capacity(self.shipping.len());
        for group in &self.shipping {
            let (merchant_discount, subsidy_discount) = split_funding(&group.discounts, currency)?;
            let taxable = taxable_base(group.price, &group.discounts)?;
            let result = tax_quote.line(group.group_id.as_str()).ok_or_else(|| {
                Error::internal(format!("tax result missing for {}", group.group_id))
            })?;

            let net_of_discounts =
                group.price.try_sub(merchant_discount)?.try_sub(subsidy_discount)?;
            let (customer_total, merchant_gross) = match tax_mode {
                TaxMode::Exclusive => (
                    net_of_discounts.try_add(result.tax)?,
                    group.price.try_sub(merchant_discount)?.try_add(result.tax)?,
                ),
                TaxMode::Inclusive => (net_of_discounts, group.price.try_sub(merchant_discount)?),
            };

            priced_shipping.push(PricedShipping {
                group_id: group.group_id.clone(),
                shop_id: group.shop_id.clone(),
                price: group.price,
                discounts: group.discounts.clone(),
                merchant_discount,
                subsidy_discount,
                taxable_base: taxable,
                tax: result.tax,
                customer_total,
                merchant_gross,
            });
        }

        // Aggregate per shop.
        let mut shop_totals: Vec<ShopTotals> = Vec::new();
        {
            for line in &priced_lines {
                let index = upsert_shop(&mut shop_totals, &line.shop_id, zero);
                let totals = &mut shop_totals[index];
                totals.subtotal = totals.subtotal.try_add(line.subtotal)?;
                totals.merchant_discount =
                    totals.merchant_discount.try_add(line.merchant_discount)?;
                totals.subsidy_discount = totals.subsidy_discount.try_add(line.subsidy_discount)?;
                totals.tax = totals.tax.try_add(line.tax)?;
                totals.customer_total = totals.customer_total.try_add(line.customer_total)?;
                totals.merchant_gross = totals.merchant_gross.try_add(line.merchant_gross)?;
            }
            for group in &priced_shipping {
                let index = upsert_shop(&mut shop_totals, &group.shop_id, zero);
                let totals = &mut shop_totals[index];
                totals.shipping = totals.shipping.try_add(group.price)?;
                totals.merchant_discount =
                    totals.merchant_discount.try_add(group.merchant_discount)?;
                totals.subsidy_discount =
                    totals.subsidy_discount.try_add(group.subsidy_discount)?;
                totals.tax = totals.tax.try_add(group.tax)?;
                totals.customer_total = totals.customer_total.try_add(group.customer_total)?;
                totals.merchant_gross = totals.merchant_gross.try_add(group.merchant_gross)?;
            }
        }

        // Aggregate per funder.
        let mut subsidies: Vec<SubsidyTotals> = Vec::new();
        {
            for line in &priced_lines {
                for applied in &line.discounts {
                    if let Some(funder) = applied.funder() {
                        record_subsidy(
                            &mut subsidies,
                            funder,
                            &line.shop_id,
                            applied.amount,
                            zero,
                        )?;
                    }
                }
            }
            for group in &priced_shipping {
                for applied in &group.discounts {
                    if let Some(funder) = applied.funder() {
                        record_subsidy(
                            &mut subsidies,
                            funder,
                            &group.shop_id,
                            applied.amount,
                            zero,
                        )?;
                    }
                }
            }
        }

        let totals = QuoteTotals {
            subtotal: Money::sum(priced_lines.iter().map(|l| l.subtotal), currency)?,
            shipping: Money::sum(priced_shipping.iter().map(|s| s.price), currency)?,
            merchant_discount: Money::sum(
                priced_lines
                    .iter()
                    .map(|l| l.merchant_discount)
                    .chain(priced_shipping.iter().map(|s| s.merchant_discount)),
                currency,
            )?,
            subsidy_discount: Money::sum(
                priced_lines
                    .iter()
                    .map(|l| l.subsidy_discount)
                    .chain(priced_shipping.iter().map(|s| s.subsidy_discount)),
                currency,
            )?,
            tax: Money::sum(
                priced_lines.iter().map(|l| l.tax).chain(priced_shipping.iter().map(|s| s.tax)),
                currency,
            )?,
            total: Money::sum(
                priced_lines
                    .iter()
                    .map(|l| l.customer_total)
                    .chain(priced_shipping.iter().map(|s| s.customer_total)),
                currency,
            )?,
            merchant_gross: Money::sum(
                priced_lines
                    .iter()
                    .map(|l| l.merchant_gross)
                    .chain(priced_shipping.iter().map(|s| s.merchant_gross)),
                currency,
            )?,
        };

        let quote = Quote {
            currency,
            tax_mode,
            lines: priced_lines,
            shipping: priced_shipping,
            totals,
            shop_totals,
            subsidies,
            rejected_discounts,
            priced_at,
        };
        quote.verify()?;
        Ok(quote)
    }
}

fn upsert_shop(totals: &mut Vec<ShopTotals>, shop_id: &ShopId, zero: Money) -> usize {
    if let Some(index) = totals.iter().position(|entry| &entry.shop_id == shop_id) {
        return index;
    }
    totals.push(ShopTotals {
        shop_id: shop_id.clone(),
        subtotal: zero,
        shipping: zero,
        merchant_discount: zero,
        subsidy_discount: zero,
        tax: zero,
        customer_total: zero,
        merchant_gross: zero,
    });
    totals.len() - 1
}

fn record_subsidy(
    subsidies: &mut Vec<SubsidyTotals>,
    funder: &AccountId,
    shop: &ShopId,
    amount: Money,
    zero: Money,
) -> Result<()> {
    let index = match subsidies.iter().position(|entry| &entry.funder == funder) {
        Some(index) => index,
        None => {
            subsidies.push(SubsidyTotals {
                funder: funder.clone(),
                amount: zero,
                per_shop: BTreeMap::new(),
            });
            subsidies.len() - 1
        }
    };
    let entry = &mut subsidies[index];
    entry.amount = entry.amount.try_add(amount)?;
    let per_shop = entry.per_shop.entry(shop.to_string()).or_insert(zero);
    *per_shop = per_shop.try_add(amount)?;
    Ok(())
}

fn taxable_base(base: Money, discounts: &[AppliedDiscount]) -> Result<Money> {
    let mut result = base;
    for applied in discounts.iter().filter(|applied| applied.reduced_taxable_base) {
        result = result.try_sub(applied.amount)?;
    }
    Ok(result.clamp_non_negative())
}

fn split_funding(discounts: &[AppliedDiscount], currency: Currency) -> Result<(Money, Money)> {
    let mut merchant = Money::zero(currency);
    let mut subsidy = Money::zero(currency);
    for applied in discounts {
        if applied.funding.is_subsidy() {
            subsidy = subsidy.try_add(applied.amount)?;
        } else {
            merchant = merchant.try_add(applied.amount)?;
        }
    }
    Ok((merchant, subsidy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Address, CountryCode};
    use crate::cart::{CartItem, FulfillmentMethod, FulfillmentSelection};

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn engine() -> PricingEngine {
        let tax = RateTableTaxCalculator::with_rules([TaxRule::new(
            "CA State Tax",
            "state",
            Jurisdiction::region(CountryCode::US, "CA"),
            1_000, // a round 10 % keeps the assertions readable
        )]);
        PricingEngine::new(Arc::new(tax))
    }

    fn cart_with(items: Vec<CartItem>) -> Cart {
        let mut cart = Cart::new(Currency::USD);
        cart.set_shipping_address(Address::new(CountryCode::US).with_region("CA"));
        for item in items {
            cart.add_item(item).unwrap();
        }
        cart
    }

    #[tokio::test]
    async fn prices_a_simple_single_shop_cart() {
        let cart = cart_with(vec![
            CartItem::new("shop-1", "tee", "T-shirt", usd(2_000), 2).unwrap(),
            CartItem::new("shop-1", "mug", "Mug", usd(1_000), 1).unwrap(),
        ]);
        let quote = engine().quote(&cart, &[]).await.unwrap();

        assert_eq!(quote.totals.subtotal, usd(5_000));
        assert_eq!(quote.totals.tax, usd(500));
        assert_eq!(quote.amount_due(), usd(5_500));
        assert_eq!(quote.totals.merchant_gross, usd(5_500));
        quote.verify().unwrap();
    }

    #[tokio::test]
    async fn merchant_discount_lowers_the_tax_base() {
        let cart = cart_with(vec![CartItem::new("shop-1", "tee", "T", usd(10_000), 1).unwrap()]);
        let discount = Discount::percentage_off("SAVE10", "10% off", 1_000);
        let quote = engine().quote(&cart, &[discount]).await.unwrap();

        assert_eq!(quote.totals.merchant_discount, usd(1_000));
        assert_eq!(quote.lines[0].taxable_base, usd(9_000));
        assert_eq!(quote.totals.tax, usd(900));
        assert_eq!(quote.amount_due(), usd(9_900));
        // The shop absorbed the discount, so it is owed the discounted price.
        assert_eq!(quote.totals.merchant_gross, usd(9_900));
    }

    #[tokio::test]
    async fn subsidy_leaves_the_tax_base_and_the_shop_whole() {
        let cart = cart_with(vec![CartItem::new("shop-1", "tee", "T", usd(10_000), 1).unwrap()]);
        let subsidy = Discount::amount_off("PLATFORM20", "Platform promotion", usd(2_000))
            .funded_by(AccountId::from_string("acct_platform"), "welcome");
        let quote = engine().quote(&cart, &[subsidy]).await.unwrap();

        // Tax is still charged on the full 100.00 because the shop's
        // consideration did not change.
        assert_eq!(quote.lines[0].taxable_base, usd(10_000));
        assert_eq!(quote.totals.tax, usd(1_000));
        assert_eq!(quote.amount_due(), usd(9_000)); // 100 - 20 + 10 tax
        assert_eq!(quote.totals.merchant_gross, usd(11_000));
        assert_eq!(quote.subsidies.len(), 1);
        assert_eq!(quote.subsidies[0].amount, usd(2_000));
        quote.verify().unwrap();
    }

    #[tokio::test]
    async fn stacked_discounts_apply_in_priority_order() {
        let cart = cart_with(vec![CartItem::new("shop-1", "tee", "T", usd(10_000), 1).unwrap()]);
        let ten_off = Discount::amount_off("TEN", "$10 off", usd(1_000)).with_priority(1);
        let then_half = Discount::percentage_off("HALF", "50% off", 5_000).with_priority(2);

        let quote = engine().quote(&cart, &[then_half, ten_off]).await.unwrap();
        // $100 - $10 = $90, then 50 % of the remaining $90 = $45.
        assert_eq!(quote.totals.merchant_discount, usd(5_500));
        assert_eq!(quote.lines[0].taxable_base, usd(4_500));
    }

    #[tokio::test]
    async fn order_level_discount_is_prorated_across_shops_without_losing_a_cent() {
        let cart = cart_with(vec![
            CartItem::new("shop-1", "a", "A", usd(3_333), 1).unwrap(),
            CartItem::new("shop-2", "b", "B", usd(3_333), 1).unwrap(),
            CartItem::new("shop-3", "c", "C", usd(3_334), 1).unwrap(),
        ]);
        let discount = Discount::amount_off("TEN", "$10 off", usd(1_000));
        let quote = engine().quote(&cart, &[discount]).await.unwrap();

        let allocated =
            Money::sum(quote.lines.iter().map(|line| line.merchant_discount), Currency::USD)
                .unwrap();
        assert_eq!(allocated, usd(1_000), "allocation must be exact");
        assert_eq!(quote.shop_totals.len(), 3);
        quote.verify().unwrap();
    }

    #[tokio::test]
    async fn shop_scoped_discount_only_touches_that_shop() {
        let cart = cart_with(vec![
            CartItem::new("shop-1", "a", "A", usd(5_000), 1).unwrap(),
            CartItem::new("shop-2", "b", "B", usd(5_000), 1).unwrap(),
        ]);
        let discount = Discount::percentage_off("S1", "Shop 1 sale", 2_000)
            .with_scope(DiscountScope::Shop { shop_id: ShopId::from_string("shop-1") });
        let quote = engine().quote(&cart, &[discount]).await.unwrap();

        assert_eq!(quote.shop(&ShopId::from_string("shop-1")).unwrap().merchant_discount, usd(1_000));
        assert_eq!(quote.shop(&ShopId::from_string("shop-2")).unwrap().merchant_discount, usd(0));
    }

    #[tokio::test]
    async fn free_shipping_waives_only_matching_groups() {
        let mut cart = cart_with(vec![
            CartItem::new("shop-1", "a", "A", usd(5_000), 1)
                .unwrap()
                .with_fulfillment(FulfillmentSelection::new(FulfillmentMethod::Shipping {
                    carrier: "ups".into(),
                    service: "ground".into(),
                })),
            CartItem::new("shop-2", "b", "B", usd(5_000), 1)
                .unwrap()
                .with_fulfillment(FulfillmentSelection::new(FulfillmentMethod::Shipping {
                    carrier: "ups".into(),
                    service: "ground".into(),
                })),
        ]);
        cart.regroup_fulfillment(|_| Ok(usd(1_000))).unwrap();

        let discount = Discount {
            id: DiscountId::new(),
            code: Some("FREESHIP1".into()),
            description: "Free shipping from shop 1".into(),
            value: DiscountValue::FreeShipping,
            scope: DiscountScope::Shop { shop_id: ShopId::from_string("shop-1") },
            funding: DiscountFunding::Merchant,
            priority: 0,
            stackable: true,
            conditions: DiscountConditions::default(),
            reduces_taxable_base_override: None,
        };

        let quote = engine().quote(&cart, &[discount]).await.unwrap();
        assert_eq!(quote.totals.shipping, usd(2_000));
        assert_eq!(quote.totals.merchant_discount, usd(1_000));
        let shop1 = quote.shipping.iter().find(|s| s.shop_id.as_str() == "shop-1").unwrap();
        assert_eq!(shop1.customer_total, usd(0));
        let shop2 = quote.shipping.iter().find(|s| s.shop_id.as_str() == "shop-2").unwrap();
        assert_eq!(shop2.customer_total, usd(1_100));
    }

    #[tokio::test]
    async fn ineligible_codes_are_reported_not_fatal() {
        let cart = cart_with(vec![CartItem::new("shop-1", "a", "A", usd(1_000), 1).unwrap()]);
        let discount = Discount::amount_off("BIGSPEND", "$5 off over $500", usd(500))
            .with_conditions(DiscountConditions {
                minimum_subtotal: Some(usd(50_000)),
                ..Default::default()
            });
        let quote = engine().quote(&cart, &[discount]).await.unwrap();

        assert_eq!(quote.totals.merchant_discount, usd(0));
        assert_eq!(quote.rejected_discounts.len(), 1);
        assert_eq!(quote.rejected_discounts[0].code, "BIGSPEND");
        assert_eq!(quote.amount_due(), usd(1_100));
    }

    #[tokio::test]
    async fn a_discount_can_never_exceed_the_cart_value() {
        let cart = cart_with(vec![CartItem::new("shop-1", "a", "A", usd(1_000), 1).unwrap()]);
        let discount = Discount::amount_off("HUGE", "$500 off", usd(50_000));
        let quote = engine().quote(&cart, &[discount]).await.unwrap();

        assert_eq!(quote.totals.merchant_discount, usd(1_000));
        assert_eq!(quote.amount_due(), usd(0));
        quote.verify().unwrap();
    }

    #[tokio::test]
    async fn inclusive_tax_mode_extracts_instead_of_adding() {
        let tax = RateTableTaxCalculator::with_rules([TaxRule::new(
            "VAT",
            "vat",
            Jurisdiction::country(CountryCode::DE),
            2_000,
        )]);
        let engine = PricingEngine::with_config(
            Arc::new(tax),
            PricingConfig {
                tax_mode: TaxMode::Inclusive,
                rounding: Rounding::HalfUp,
                fallback_jurisdiction: Jurisdiction::country(CountryCode::DE),
            },
        );
        let mut cart = Cart::new(Currency::EUR);
        cart.set_shipping_address(Address::new(CountryCode::DE));
        cart.add_item(
            CartItem::new("shop-1", "a", "A", Money::from_minor(12_000, Currency::EUR), 1).unwrap(),
        )
        .unwrap();

        let quote = engine.quote(&cart, &[]).await.unwrap();
        assert_eq!(quote.amount_due(), Money::from_minor(12_000, Currency::EUR));
        assert_eq!(quote.totals.tax, Money::from_minor(2_000, Currency::EUR));
        quote.verify().unwrap();
    }

    #[tokio::test]
    async fn empty_cart_is_rejected() {
        let cart = Cart::new(Currency::USD);
        assert!(engine().quote(&cart, &[]).await.is_err());
    }
}
