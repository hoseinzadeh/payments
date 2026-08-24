//! Refunds: giving money back, correctly, to everyone involved.
//!
//! Refunding a marketplace order is not "send `n` back to the card". The same
//! amount has to be unwound across three dimensions at once:
//!
//! * **Shops** — each shop's share, and the tax it no longer has to remit.
//! * **Funders** — a subsidy that paid for a returned item must be reclaimed,
//!   otherwise the platform has funded a product the shopper does not have.
//! * **Tenders** — the shopper paid with a gift card *and* a card; each has to
//!   get back exactly what it put in, or the balances drift.
//!
//! [`RefundPlan::build`] computes all three from the order's stored quote,
//! settlement and tender plans, and proves the result balances before anything
//! is sent to a gateway.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::gateway::RefundReason;
use crate::ids::{AccountId, LineItemId, RefundId, ShopId};
use crate::money::{Currency, Money, allocate, allocate_by_weights};
use crate::order::Order;
use crate::payment::tender::TenderKind;

/// Which units of a line to refund.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundLineRequest {
    /// Line to refund.
    pub line_id: LineItemId,
    /// How many units.
    pub quantity: u32,
}

/// What to refund.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RefundScope {
    /// Everything that has not already been refunded, including shipping and
    /// any subsidies.
    Full,
    /// Specific units of specific lines.
    Lines {
        /// Lines and quantities.
        lines: Vec<RefundLineRequest>,
    },
    /// A flat amount, prorated across shops by what each is still owed.
    ///
    /// Subsidies are **not** reclaimed for flat-amount refunds: there is no
    /// principled way to decide whose subsidy a goodwill gesture consumes, so
    /// the platform absorbs it. Use [`RefundScope::Lines`] when a specific item
    /// is being returned.
    Amount {
        /// How much to return to the shopper.
        amount: Money,
    },
}

/// A refund instruction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefundRequest {
    /// What to refund.
    pub scope: RefundScope,
    /// Why.
    #[serde(default)]
    pub reason: RefundReason,
    /// Also refund shipping charges for fully-refunded shipments.
    #[serde(default = "default_true")]
    pub refund_shipping: bool,
    /// Give the platform commission back to the shop proportionally.
    #[serde(default = "default_true")]
    pub refund_platform_fee: bool,
    /// Makes retries safe.
    pub idempotency_key: String,
    /// Operator note for the audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

fn default_true() -> bool {
    true
}

impl RefundRequest {
    /// Refund everything left on the order.
    pub fn full(idempotency_key: impl Into<String>) -> Self {
        Self {
            scope: RefundScope::Full,
            reason: RefundReason::RequestedByCustomer,
            refund_shipping: true,
            refund_platform_fee: true,
            idempotency_key: idempotency_key.into(),
            note: None,
        }
    }

    /// Refund specific units.
    pub fn lines(idempotency_key: impl Into<String>, lines: Vec<RefundLineRequest>) -> Self {
        Self {
            scope: RefundScope::Lines { lines },
            reason: RefundReason::RequestedByCustomer,
            refund_shipping: false,
            refund_platform_fee: true,
            idempotency_key: idempotency_key.into(),
            note: None,
        }
    }

    /// Refund a flat amount.
    pub fn amount(idempotency_key: impl Into<String>, amount: Money) -> Self {
        Self {
            scope: RefundScope::Amount { amount },
            reason: RefundReason::RequestedByCustomer,
            refund_shipping: false,
            refund_platform_fee: true,
            idempotency_key: idempotency_key.into(),
            note: None,
        }
    }

    /// Builder: set the reason.
    pub fn because(mut self, reason: RefundReason) -> Self {
        self.reason = reason;
        self
    }

    /// Builder: include or exclude shipping.
    pub fn with_shipping(mut self, refund_shipping: bool) -> Self {
        self.refund_shipping = refund_shipping;
        self
    }
}

/// The refunded portion of one line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefundedLine {
    /// The line.
    pub line_id: LineItemId,
    /// Shop that sold it.
    pub shop_id: ShopId,
    /// Units refunded.
    pub quantity: u32,
    /// Amount returned to the shopper.
    pub customer_amount: Money,
    /// Subsidy reclaimed from funders.
    pub subsidy_amount: Money,
    /// Tax no longer due.
    pub tax_amount: Money,
    /// Amount clawed back from the shop.
    pub merchant_amount: Money,
}

/// What one shop gives back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShopRefund {
    /// The shop.
    pub shop_id: ShopId,
    /// Its connected account.
    pub account_id: AccountId,
    /// Total clawed back from the shop, tax included.
    pub gross: Money,
    /// Tax component of `gross`.
    pub tax: Money,
    /// Platform commission returned to the shop.
    pub platform_fee_returned: Money,
    /// Net debit against the shop's balance.
    pub net: Money,
    /// Portion returned to the shopper.
    pub customer_funded: Money,
    /// Portion returned to funders.
    pub subsidy_funded: Money,
}

/// What one tender gets back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenderRefund {
    /// Index of the tender in [`Order::tenders`].
    pub tender_index: usize,
    /// The instrument.
    pub kind: TenderKind,
    /// Amount returned to it.
    pub amount: Money,
    /// Per-shop attribution of that amount.
    pub shop_allocation: BTreeMap<String, Money>,
}

/// What one funder gets back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunderRefund {
    /// The funder.
    pub funder: AccountId,
    /// Amount reclaimed.
    pub amount: Money,
}

/// A complete, balanced refund.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefundPlan {
    /// Currency.
    pub currency: Currency,
    /// Total returned to the shopper.
    pub total: Money,
    /// Tax included in `total` plus subsidy reclaims.
    pub tax_refunded: Money,
    /// Total reclaimed from funders.
    pub subsidy_reclaimed: Money,
    /// Refunded units.
    pub lines: Vec<RefundedLine>,
    /// Shipments whose charges were refunded.
    pub shipping_groups: Vec<crate::ids::FulfillmentGroupId>,
    /// Per-shop clawbacks.
    pub shops: Vec<ShopRefund>,
    /// Per-tender returns.
    pub tenders: Vec<TenderRefund>,
    /// Per-funder reclaims.
    pub funders: Vec<FunderRefund>,
}

impl RefundPlan {
    /// Compute a balanced refund for `order`.
    pub fn build(order: &Order, request: &RefundRequest) -> Result<Self> {
        let currency = order.currency;
        let zero = Money::zero(currency);

        let mut lines: Vec<RefundedLine> = Vec::new();
        let mut shipping_groups = Vec::new();
        let mut shop_customer: BTreeMap<String, Money> = BTreeMap::new();
        let mut shop_subsidy: BTreeMap<String, Money> = BTreeMap::new();
        let mut shop_tax: BTreeMap<String, Money> = BTreeMap::new();
        let mut shop_gross: BTreeMap<String, Money> = BTreeMap::new();
        let mut funder_totals: BTreeMap<String, Money> = BTreeMap::new();

        match &request.scope {
            RefundScope::Full | RefundScope::Lines { .. } => {
                let requested: Option<BTreeMap<String, u32>> = match &request.scope {
                    RefundScope::Lines { lines } => Some(
                        lines
                            .iter()
                            .map(|line| (line.line_id.to_string(), line.quantity))
                            .collect(),
                    ),
                    _ => None,
                };

                for priced in &order.quote.lines {
                    let already = order.refunded_quantity(&priced.line_id);
                    let remaining = priced.quantity.saturating_sub(already);
                    let wanted = match &requested {
                        Some(map) => map.get(priced.line_id.as_str()).copied().unwrap_or(0),
                        None => remaining,
                    };
                    if wanted == 0 {
                        continue;
                    }
                    if wanted > remaining {
                        return Err(Error::validation(format!(
                            "cannot refund {wanted} units of {}: only {remaining} remain",
                            priced.line_id
                        )));
                    }

                    // Split each line total into exact per-unit shares so that
                    // repeated partial refunds always sum back to the whole.
                    let units = priced.quantity as usize;
                    let range = already as usize..(already + wanted) as usize;
                    let customer =
                        sum_range(&allocate(priced.customer_total, &vec![1; units])?, &range, currency)?;
                    let subsidy =
                        sum_range(&allocate(priced.subsidy_discount, &vec![1; units])?, &range, currency)?;
                    let tax = sum_range(&allocate(priced.tax, &vec![1; units])?, &range, currency)?;
                    let merchant =
                        sum_range(&allocate(priced.merchant_gross, &vec![1; units])?, &range, currency)?;

                    add(&mut shop_customer, &priced.shop_id, customer, zero)?;
                    add(&mut shop_subsidy, &priced.shop_id, subsidy, zero)?;
                    add(&mut shop_tax, &priced.shop_id, tax, zero)?;
                    add(&mut shop_gross, &priced.shop_id, merchant, zero)?;

                    // Attribute the reclaimed subsidy to the funders that paid.
                    if subsidy.is_positive() {
                        let weights: Vec<Money> = priced
                            .discounts
                            .iter()
                            .filter(|applied| applied.funding.is_subsidy())
                            .map(|applied| applied.amount)
                            .collect();
                        let funders: Vec<&AccountId> = priced
                            .discounts
                            .iter()
                            .filter_map(|applied| applied.funder())
                            .collect();
                        let shares = allocate_by_weights(subsidy, &weights)?;
                        for (funder, share) in funders.into_iter().zip(shares) {
                            let entry =
                                funder_totals.entry(funder.to_string()).or_insert(zero);
                            *entry = entry.try_add(share)?;
                        }
                    }

                    lines.push(RefundedLine {
                        line_id: priced.line_id.clone(),
                        shop_id: priced.shop_id.clone(),
                        quantity: wanted,
                        customer_amount: customer,
                        subsidy_amount: subsidy,
                        tax_amount: tax,
                        merchant_amount: merchant,
                    });
                }

                if request.refund_shipping {
                    for shipping in &order.quote.shipping {
                        if order
                            .refunds
                            .iter()
                            .any(|record| record.plan.shipping_groups.contains(&shipping.group_id))
                        {
                            continue;
                        }
                        // Only refund shipping once every item in the shipment
                        // has been returned.
                        let fully_returned = order
                            .quote
                            .lines
                            .iter()
                            .filter(|line| {
                                line.fulfillment_group_id.as_ref() == Some(&shipping.group_id)
                            })
                            .all(|line| {
                                let refunded_now = lines
                                    .iter()
                                    .filter(|refunded| refunded.line_id == line.line_id)
                                    .map(|refunded| refunded.quantity)
                                    .sum::<u32>();
                                order.refunded_quantity(&line.line_id) + refunded_now
                                    >= line.quantity
                            });
                        if !fully_returned {
                            continue;
                        }
                        if shipping.customer_total.is_zero() && shipping.merchant_gross.is_zero() {
                            continue;
                        }

                        add(&mut shop_customer, &shipping.shop_id, shipping.customer_total, zero)?;
                        add(&mut shop_subsidy, &shipping.shop_id, shipping.subsidy_discount, zero)?;
                        add(&mut shop_tax, &shipping.shop_id, shipping.tax, zero)?;
                        add(&mut shop_gross, &shipping.shop_id, shipping.merchant_gross, zero)?;
                        for applied in &shipping.discounts {
                            if let Some(funder) = applied.funder() {
                                let entry =
                                    funder_totals.entry(funder.to_string()).or_insert(zero);
                                *entry = entry.try_add(applied.amount)?;
                            }
                        }
                        shipping_groups.push(shipping.group_id.clone());
                    }
                }
            }
            RefundScope::Amount { amount } => {
                if !amount.is_positive() {
                    return Err(Error::validation("refund amount must be positive"));
                }
                let refundable = order.refundable_amount()?;
                if *amount > refundable {
                    return Err(Error::validation(format!(
                        "cannot refund {amount}: only {refundable} remains refundable"
                    )));
                }
                let weights: Vec<Money> = order
                    .settlement
                    .shops
                    .iter()
                    .map(|shop| {
                        remaining_customer_for_shop(order, &shop.shop_id)
                            .unwrap_or_else(|_| Money::zero(currency))
                    })
                    .collect();
                let shares = allocate_by_weights(*amount, &weights)?;
                for (settlement, share) in order.settlement.shops.iter().zip(shares) {
                    if share.is_zero() {
                        continue;
                    }
                    add(&mut shop_customer, &settlement.shop_id, share, zero)?;
                    add(&mut shop_gross, &settlement.shop_id, share, zero)?;
                    // Tax is prorated out of the shop's own effective tax rate.
                    let tax = if settlement.gross.is_zero() {
                        zero
                    } else {
                        share.mul_ratio(
                            settlement.tax.minor(),
                            settlement.gross.minor(),
                            crate::money::Rounding::HalfUp,
                        )?
                    };
                    add(&mut shop_tax, &settlement.shop_id, tax, zero)?;
                }
            }
        }

        let total = Money::sum(shop_customer.values().copied(), currency)?;
        if !total.is_positive() {
            return Err(Error::validation("this refund would return nothing"));
        }
        let refundable = order.refundable_amount()?;
        if total > refundable {
            return Err(Error::validation(format!(
                "cannot refund {total}: only {refundable} remains refundable"
            )));
        }

        // Per-shop aggregation, including the returned platform commission.
        let mut shops = Vec::new();
        for settlement in &order.settlement.shops {
            let key = settlement.shop_id.to_string();
            let customer_funded = shop_customer.get(&key).copied().unwrap_or(zero);
            let subsidy_funded = shop_subsidy.get(&key).copied().unwrap_or(zero);
            let gross = shop_gross
                .get(&key)
                .copied()
                .unwrap_or(zero)
                .try_max(customer_funded.try_add(subsidy_funded)?)?;
            if gross.is_zero() {
                continue;
            }
            let platform_fee_returned = if request.refund_platform_fee
                && settlement.platform_fee.is_positive()
                && settlement.gross.is_positive()
            {
                settlement.platform_fee.mul_ratio(
                    gross.minor(),
                    settlement.gross.minor(),
                    crate::money::Rounding::HalfUp,
                )?
            } else {
                zero
            };
            shops.push(ShopRefund {
                shop_id: settlement.shop_id.clone(),
                account_id: settlement.account_id.clone(),
                gross,
                tax: shop_tax.get(&key).copied().unwrap_or(zero),
                platform_fee_returned,
                net: gross.try_sub(platform_fee_returned)?,
                customer_funded,
                subsidy_funded,
            });
        }

        // Split what goes back to the shopper across the tenders that paid,
        // proportionally to what each tender still has outstanding per shop.
        let mut tender_refunds: BTreeMap<usize, TenderRefund> = BTreeMap::new();
        for shop in &shops {
            if !shop.customer_funded.is_positive() {
                continue;
            }
            let mut indices = Vec::new();
            let mut weights = Vec::new();
            for (index, tender) in order.tenders.iter().enumerate() {
                let paid = tender.amount_for_shop(&shop.shop_id, currency);
                let already = order.refunded_from_tender(index, &shop.shop_id)?;
                let outstanding = paid.try_sub(already)?.clamp_non_negative();
                if outstanding.is_positive() {
                    indices.push(index);
                    weights.push(outstanding);
                }
            }
            let available = Money::sum(weights.iter().copied(), currency)?;
            if shop.customer_funded > available {
                return Err(Error::internal(format!(
                    "shop {} needs {} refunded but its tenders only hold {available}",
                    shop.shop_id, shop.customer_funded
                )));
            }
            let shares = allocate_by_weights(shop.customer_funded, &weights)?;
            for (index, share) in indices.into_iter().zip(shares) {
                if share.is_zero() {
                    continue;
                }
                let entry = tender_refunds.entry(index).or_insert_with(|| TenderRefund {
                    tender_index: index,
                    kind: order.tenders[index].kind.clone(),
                    amount: zero,
                    shop_allocation: BTreeMap::new(),
                });
                entry.amount = entry.amount.try_add(share)?;
                let allocation =
                    entry.shop_allocation.entry(shop.shop_id.to_string()).or_insert(zero);
                *allocation = allocation.try_add(share)?;
            }
        }

        let funders = funder_totals
            .into_iter()
            .map(|(funder, amount)| FunderRefund {
                funder: AccountId::from_string(funder),
                amount,
            })
            .filter(|refund| refund.amount.is_positive())
            .collect::<Vec<_>>();

        let plan = RefundPlan {
            currency,
            total,
            tax_refunded: Money::sum(shops.iter().map(|shop| shop.tax), currency)?,
            subsidy_reclaimed: Money::sum(funders.iter().map(|f| f.amount), currency)?,
            lines,
            shipping_groups,
            shops,
            tenders: tender_refunds.into_values().collect(),
            funders,
        };
        plan.verify()?;
        Ok(plan)
    }

    /// Assert the refund balances.
    pub fn verify(&self) -> Result<()> {
        let customer =
            Money::sum(self.shops.iter().map(|shop| shop.customer_funded), self.currency)?;
        if customer != self.total {
            return Err(Error::internal(format!(
                "refund shop shares {customer} do not sum to the total {}",
                self.total
            )));
        }
        let tendered = Money::sum(self.tenders.iter().map(|t| t.amount), self.currency)?;
        if tendered != self.total {
            return Err(Error::internal(format!(
                "refund tenders return {tendered} but {} is being refunded",
                self.total
            )));
        }
        for shop in &self.shops {
            let funded = shop.customer_funded.try_add(shop.subsidy_funded)?;
            if funded != shop.gross {
                return Err(Error::internal(format!(
                    "shop {} refund funding {funded} does not match gross {}",
                    shop.shop_id, shop.gross
                )));
            }
        }
        Ok(())
    }

    /// The amount to return through a specific tender.
    pub fn amount_for_tender(&self, tender_index: usize) -> Money {
        self.tenders
            .iter()
            .find(|tender| tender.tender_index == tender_index)
            .map(|tender| tender.amount)
            .unwrap_or_else(|| Money::zero(self.currency))
    }
}

/// A refund as stored on an order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefundRecord {
    /// Identifier.
    pub id: RefundId,
    /// The computed plan.
    pub plan: RefundPlan,
    /// Why it happened.
    pub reason: RefundReason,
    /// Idempotency key of the request that produced it.
    pub idempotency_key: String,
    /// Gateway refund identifiers, one per gateway tender that was refunded.
    #[serde(default)]
    pub gateway_references: Vec<String>,
    /// Operator note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// When it was issued.
    pub created_at: DateTime<Utc>,
}

impl RefundRecord {
    /// Wrap a plan into a stored record.
    pub fn new(plan: RefundPlan, request: &RefundRequest) -> Self {
        Self {
            id: RefundId::new(),
            plan,
            reason: request.reason,
            idempotency_key: request.idempotency_key.clone(),
            gateway_references: Vec::new(),
            note: request.note.clone(),
            created_at: Utc::now(),
        }
    }
}

fn sum_range(shares: &[Money], range: &std::ops::Range<usize>, currency: Currency) -> Result<Money> {
    let slice = shares
        .get(range.clone())
        .ok_or_else(|| Error::internal("refund unit range out of bounds"))?;
    Money::sum(slice.iter().copied(), currency)
}

fn add(
    map: &mut BTreeMap<String, Money>,
    shop: &ShopId,
    amount: Money,
    zero: Money,
) -> Result<()> {
    let entry = map.entry(shop.to_string()).or_insert(zero);
    *entry = entry.try_add(amount)?;
    Ok(())
}

fn remaining_customer_for_shop(order: &Order, shop_id: &ShopId) -> Result<Money> {
    let paid = order
        .settlement
        .shop(shop_id)
        .map(|shop| shop.funded_by_customer)
        .unwrap_or_else(|| Money::zero(order.currency));
    let mut refunded = Money::zero(order.currency);
    for record in &order.refunds {
        for shop in &record.plan.shops {
            if &shop.shop_id == shop_id {
                refunded = refunded.try_add(shop.customer_funded)?;
            }
        }
    }
    paid.try_sub(refunded).map(|amount| amount.clamp_non_negative())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Address, CountryCode, Jurisdiction};
    use crate::cart::{Cart, CartItem};
    use crate::gateway::{GatewayId, InstrumentRef};
    use crate::ids::GiftCardId;
    use crate::money::Currency;
    use crate::order::Order;
    use crate::payment::split::{PlatformFeePolicy, ShopAccounts, SettlementPlan};
    use crate::payment::tender::{TenderOffer, TenderPlan};
    use crate::pricing::{Discount, PricingEngine, RateTableTaxCalculator, TaxRule};
    use std::sync::Arc;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    async fn build_order(
        items: &[(&str, i64, u32)],
        discounts: Vec<Discount>,
        offers: Vec<TenderOffer>,
        fees: PlatformFeePolicy,
    ) -> Order {
        let tax = RateTableTaxCalculator::with_rules([TaxRule::new(
            "CA",
            "state",
            Jurisdiction::region(CountryCode::US, "CA"),
            1_000,
        )]);
        let engine = PricingEngine::new(Arc::new(tax));
        let mut cart = Cart::new(Currency::USD);
        cart.set_shipping_address(Address::new(CountryCode::US).with_region("CA"));
        for (shop, price, qty) in items {
            cart.add_item(CartItem::new(*shop, "sku", "Item", usd(*price), *qty).unwrap())
                .unwrap();
        }
        let quote = engine.quote(&cart, &discounts).await.unwrap();
        let settlement =
            SettlementPlan::from_quote(&quote, &ShopAccounts::new(), &fees).unwrap();
        let tenders = TenderPlan::build(&quote, &offers).unwrap();
        let mut order = Order::from_quote(&cart, quote, settlement, &tenders).unwrap();
        order.transition_to(crate::order::OrderStatus::Authorized).unwrap();
        let total = order.total();
        order.record_capture(total).unwrap();
        order
    }

    fn card() -> TenderOffer {
        TenderOffer::gateway(
            GatewayId::from_static("mock"),
            InstrumentRef::SingleUseToken { token: "tok".into() },
            "visa •••• 4242",
        )
    }

    #[tokio::test]
    async fn full_refund_returns_everything_across_shops() {
        let order = build_order(
            &[("shop-1", 6_000, 1), ("shop-2", 4_000, 1)],
            vec![],
            vec![card()],
            PlatformFeePolicy::none(),
        )
        .await;

        let plan = RefundPlan::build(&order, &RefundRequest::full("r1")).unwrap();
        assert_eq!(plan.total, order.total());
        assert_eq!(plan.shops.len(), 2);
        assert_eq!(plan.tax_refunded, usd(1_000));
        assert_eq!(plan.tenders.len(), 1);
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn partial_line_refund_only_returns_those_units() {
        let order = build_order(
            &[("shop-1", 2_500, 4)],
            vec![],
            vec![card()],
            PlatformFeePolicy::none(),
        )
        .await;
        let line_id = order.quote.lines[0].line_id.clone();

        let plan = RefundPlan::build(
            &order,
            &RefundRequest::lines("r1", vec![RefundLineRequest { line_id, quantity: 1 }]),
        )
        .unwrap();

        // One of four units: 25.00 + 2.50 tax.
        assert_eq!(plan.total, usd(2_750));
        assert_eq!(plan.lines[0].quantity, 1);
        assert_eq!(plan.tax_refunded, usd(250));
    }

    #[tokio::test]
    async fn repeated_partial_refunds_sum_exactly_to_the_whole() {
        // A price that does not divide evenly by the quantity is where
        // per-unit rounding usually leaks a cent.
        let mut order = build_order(
            &[("shop-1", 3_333, 3)],
            vec![],
            vec![card()],
            PlatformFeePolicy::none(),
        )
        .await;
        let line_id = order.quote.lines[0].line_id.clone();
        let total = order.total();

        let mut refunded = usd(0);
        for index in 0..3 {
            let plan = RefundPlan::build(
                &order,
                &RefundRequest::lines(
                    format!("r{index}"),
                    vec![RefundLineRequest { line_id: line_id.clone(), quantity: 1 }],
                ),
            )
            .unwrap();
            refunded = refunded.try_add(plan.total).unwrap();
            order
                .record_refund(RefundRecord::new(plan, &RefundRequest::full("x")))
                .unwrap();
        }
        assert_eq!(refunded, total, "three unit refunds must return the whole order");
        assert_eq!(order.status, crate::order::OrderStatus::Refunded);
    }

    #[tokio::test]
    async fn refunding_more_units_than_exist_is_rejected() {
        let order =
            build_order(&[("shop-1", 1_000, 2)], vec![], vec![card()], PlatformFeePolicy::none())
                .await;
        let line_id = order.quote.lines[0].line_id.clone();
        assert!(
            RefundPlan::build(
                &order,
                &RefundRequest::lines("r1", vec![RefundLineRequest { line_id, quantity: 3 }])
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn subsidies_are_reclaimed_from_the_funder() {
        let subsidy = Discount::amount_off("SUB", "Employer benefit", usd(3_000))
            .funded_by(AccountId::from_string("acct_employer"), "benefit");
        let order = build_order(
            &[("shop-1", 10_000, 1)],
            vec![subsidy],
            vec![card()],
            PlatformFeePolicy::none(),
        )
        .await;

        let plan = RefundPlan::build(&order, &RefundRequest::full("r1")).unwrap();
        assert_eq!(plan.total, usd(8_000), "the shopper only paid 80.00");
        assert_eq!(plan.subsidy_reclaimed, usd(3_000));
        assert_eq!(plan.funders[0].funder, AccountId::from_string("acct_employer"));
        assert_eq!(plan.shops[0].gross, usd(11_000), "the shop gives back everything it got");
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn multi_tender_refunds_return_to_each_instrument() {
        let gift = TenderOffer::gift_card(GiftCardId::from_string("gc_1"), "****1234", usd(3_000));
        let order = build_order(
            &[("shop-1", 10_000, 1)],
            vec![],
            vec![gift, card()],
            PlatformFeePolicy::none(),
        )
        .await;
        assert_eq!(order.tenders.len(), 2);

        let plan = RefundPlan::build(&order, &RefundRequest::full("r1")).unwrap();
        assert_eq!(plan.total, usd(11_000));
        assert_eq!(plan.amount_for_tender(0), usd(3_000), "gift card gets its balance back");
        assert_eq!(plan.amount_for_tender(1), usd(8_000));
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn partial_refund_with_gift_card_is_prorated_across_tenders() {
        let gift = TenderOffer::gift_card(GiftCardId::from_string("gc_1"), "****1234", usd(3_000));
        let order = build_order(
            &[("shop-1", 5_000, 2)],
            vec![],
            vec![gift, card()],
            PlatformFeePolicy::none(),
        )
        .await;
        let line_id = order.quote.lines[0].line_id.clone();

        let plan = RefundPlan::build(
            &order,
            &RefundRequest::lines("r1", vec![RefundLineRequest { line_id, quantity: 1 }]),
        )
        .unwrap();

        assert_eq!(plan.total, usd(5_500));
        let returned = plan.amount_for_tender(0).try_add(plan.amount_for_tender(1)).unwrap();
        assert_eq!(returned, usd(5_500));
        assert!(plan.amount_for_tender(0).is_positive(), "gift card shares the refund");
    }

    #[tokio::test]
    async fn platform_fee_is_returned_proportionally() {
        let order = build_order(
            &[("shop-1", 10_000, 2)],
            vec![],
            vec![card()],
            PlatformFeePolicy::percentage(1_000),
        )
        .await;
        assert_eq!(order.settlement.shops[0].platform_fee, usd(2_000));
        let line_id = order.quote.lines[0].line_id.clone();

        let plan = RefundPlan::build(
            &order,
            &RefundRequest::lines("r1", vec![RefundLineRequest { line_id, quantity: 1 }]),
        )
        .unwrap();
        // Half the order is returned, so half the commission goes back.
        assert_eq!(plan.shops[0].platform_fee_returned, usd(1_000));
        assert_eq!(plan.shops[0].net, usd(10_000));
    }

    #[tokio::test]
    async fn flat_amount_refunds_are_prorated_across_shops() {
        let order = build_order(
            &[("shop-1", 6_000, 1), ("shop-2", 4_000, 1)],
            vec![],
            vec![card()],
            PlatformFeePolicy::none(),
        )
        .await;

        let plan = RefundPlan::build(&order, &RefundRequest::amount("r1", usd(1_000))).unwrap();
        assert_eq!(plan.total, usd(1_000));
        assert_eq!(plan.shops.len(), 2);
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn refunding_more_than_was_captured_is_rejected() {
        let order =
            build_order(&[("shop-1", 1_000, 1)], vec![], vec![card()], PlatformFeePolicy::none())
                .await;
        assert!(RefundPlan::build(&order, &RefundRequest::amount("r1", usd(999_999))).is_err());
    }

    #[tokio::test]
    async fn a_second_full_refund_finds_nothing_left() {
        let mut order =
            build_order(&[("shop-1", 1_000, 1)], vec![], vec![card()], PlatformFeePolicy::none())
                .await;
        let plan = RefundPlan::build(&order, &RefundRequest::full("r1")).unwrap();
        order.record_refund(RefundRecord::new(plan, &RefundRequest::full("r1"))).unwrap();
        assert_eq!(order.status, crate::order::OrderStatus::Refunded);
        assert!(RefundPlan::build(&order, &RefundRequest::full("r2")).is_err());
    }
}
