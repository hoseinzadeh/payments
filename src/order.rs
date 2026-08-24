//! Orders: the immutable record of what was bought, at what price, paid how.
//!
//! An order is created from a [`Quote`] and never re-prices itself. If a price
//! changes after the order exists, that is a new order or an amendment — a
//! captured payment must always be explainable by the numbers stored here.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::Metadata;
use crate::address::Address;
use crate::cart::{Cart, FulfillmentGroup, FulfillmentStatus};
use crate::error::{Error, Result};
use crate::ids::{CartId, CustomerId, FulfillmentGroupId, OrderId, PaymentId, ShopId};
use crate::money::{Currency, Money};
use crate::payment::refund::RefundRecord;
use crate::payment::split::SettlementPlan;
use crate::payment::tender::{PlannedTender, TenderPlan};
use crate::pricing::Quote;

/// Lifecycle of an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    /// Created, no payment attempted yet.
    Draft,
    /// Payment is in flight (3-D Secure, redirect, async method).
    PendingPayment,
    /// Funds are held but not captured.
    Authorized,
    /// Part of the order has been captured, typically after partial delivery.
    PartiallyCaptured,
    /// Fully paid.
    Paid,
    /// Fully paid and fully delivered.
    Fulfilled,
    /// Some money has been returned.
    PartiallyRefunded,
    /// Everything has been returned.
    Refunded,
    /// Cancelled before capture.
    Canceled,
    /// Payment failed and the order was abandoned.
    Failed,
}

impl OrderStatus {
    /// Whether the order can still transition to `next`.
    pub fn can_transition_to(self, next: OrderStatus) -> bool {
        use OrderStatus::*;
        matches!(
            (self, next),
            (Draft, PendingPayment)
                | (Draft, Authorized)
                | (Draft, Paid)
                | (Draft, Canceled)
                | (Draft, Failed)
                | (PendingPayment, Authorized)
                | (PendingPayment, Paid)
                | (PendingPayment, Failed)
                | (PendingPayment, Canceled)
                | (Authorized, PartiallyCaptured)
                | (Authorized, Paid)
                | (Authorized, Canceled)
                | (Authorized, Failed)
                | (PartiallyCaptured, PartiallyCaptured)
                | (PartiallyCaptured, Paid)
                | (PartiallyCaptured, PartiallyRefunded)
                | (PartiallyCaptured, Canceled)
                | (Paid, Fulfilled)
                | (Paid, PartiallyRefunded)
                | (Paid, Refunded)
                | (Fulfilled, PartiallyRefunded)
                | (Fulfilled, Refunded)
                | (PartiallyRefunded, PartiallyRefunded)
                | (PartiallyRefunded, Refunded)
                | (PartiallyRefunded, Fulfilled)
        )
    }

    /// Whether the order is finished and will not change again.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderStatus::Refunded | OrderStatus::Canceled | OrderStatus::Failed
        )
    }

    /// Whether money has been collected.
    pub fn has_funds(self) -> bool {
        matches!(
            self,
            OrderStatus::PartiallyCaptured
                | OrderStatus::Paid
                | OrderStatus::Fulfilled
                | OrderStatus::PartiallyRefunded
        )
    }
}

/// A placed order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Order {
    /// Identifier.
    pub id: OrderId,
    /// Cart the order was created from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cart_id: Option<CartId>,
    /// Buyer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<CustomerId>,
    /// Currency of every amount.
    pub currency: Currency,
    /// Lifecycle state.
    pub status: OrderStatus,
    /// The frozen pricing result.
    pub quote: Quote,
    /// Who gets paid what.
    pub settlement: SettlementPlan,
    /// Instruments used and their amounts.
    #[serde(default)]
    pub tenders: Vec<PlannedTender>,
    /// Shipments.
    #[serde(default)]
    pub fulfillment_groups: Vec<FulfillmentGroup>,
    /// Payment attempts belonging to this order.
    #[serde(default)]
    pub payments: Vec<PaymentId>,
    /// Refunds issued against this order.
    #[serde(default)]
    pub refunds: Vec<RefundRecord>,
    /// Amount captured from the shopper so far.
    pub amount_captured: Money,
    /// Amount returned to the shopper so far.
    pub amount_refunded: Money,
    /// Destination address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shipping_address: Option<Address>,
    /// Billing address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_address: Option<Address>,
    /// Free-form data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Optimistic concurrency token.
    #[serde(default)]
    pub version: u64,
}

impl Order {
    /// Create a draft order from a priced cart, a settlement plan and tenders.
    pub fn from_quote(
        cart: &Cart,
        quote: Quote,
        settlement: SettlementPlan,
        tenders: &TenderPlan,
    ) -> Result<Self> {
        if quote.currency != cart.currency {
            return Err(Error::CurrencyMismatch {
                expected: cart.currency,
                actual: quote.currency,
            });
        }
        if tenders.total_due != quote.totals.total {
            return Err(Error::validation(format!(
                "tender plan covers {} but the order total is {}",
                tenders.total_due, quote.totals.total
            )));
        }
        let now = Utc::now();
        let zero = Money::zero(quote.currency);
        Ok(Self {
            id: OrderId::new(),
            cart_id: Some(cart.id.clone()),
            customer_id: cart.customer_id.clone(),
            currency: cart.currency,
            status: OrderStatus::Draft,
            quote,
            settlement,
            tenders: tenders.tenders.clone(),
            fulfillment_groups: cart.fulfillment_groups.clone(),
            payments: Vec::new(),
            refunds: Vec::new(),
            amount_captured: zero,
            amount_refunded: zero,
            shipping_address: cart.shipping_address.clone(),
            billing_address: cart.billing_address.clone(),
            metadata: cart.metadata.clone(),
            created_at: now,
            updated_at: now,
            version: 0,
        })
    }

    /// Total the shopper owes.
    pub fn total(&self) -> Money {
        self.quote.totals.total
    }

    /// Move to a new status, rejecting illegal transitions.
    pub fn transition_to(&mut self, next: OrderStatus) -> Result<()> {
        if self.status == next {
            return Ok(());
        }
        if !self.status.can_transition_to(next) {
            return Err(Error::InvalidTransition {
                kind: "order",
                id: self.id.to_string(),
                from: format!("{:?}", self.status),
                to: format!("{next:?}"),
            });
        }
        self.status = next;
        self.touch();
        Ok(())
    }

    /// Register a payment attempt.
    pub fn attach_payment(&mut self, payment_id: PaymentId) {
        if !self.payments.contains(&payment_id) {
            self.payments.push(payment_id);
            self.touch();
        }
    }

    /// Record captured funds and advance the status accordingly.
    pub fn record_capture(&mut self, amount: Money) -> Result<()> {
        self.currency_check(amount)?;
        if !amount.is_positive() {
            return Err(Error::validation("captured amount must be positive"));
        }
        let new_total = self.amount_captured.try_add(amount)?;
        if new_total > self.total() {
            return Err(Error::validation(format!(
                "capturing {amount} would take {new_total}, more than the order total {}",
                self.total()
            )));
        }
        self.amount_captured = new_total;
        let next =
            if new_total == self.total() { OrderStatus::Paid } else { OrderStatus::PartiallyCaptured };
        if self.status.can_transition_to(next) {
            self.status = next;
        }
        self.touch();
        Ok(())
    }

    /// Amount that could still be returned to the shopper.
    pub fn refundable_amount(&self) -> Result<Money> {
        self.amount_captured.try_sub(self.amount_refunded)
    }

    /// Undo a recorded capture.
    ///
    /// Used when reversible funding — gift cards, shop credit — is handed back
    /// on cancellation. Gateway captures are never undone this way; they are
    /// refunded.
    pub fn reverse_capture(&mut self, amount: Money) -> Result<()> {
        self.currency_check(amount)?;
        if amount > self.amount_captured {
            return Err(Error::validation(format!(
                "cannot reverse {amount}: only {} was captured",
                self.amount_captured
            )));
        }
        self.amount_captured = self.amount_captured.try_sub(amount)?;
        self.touch();
        Ok(())
    }

    /// Total funded by gift cards and shop credit rather than by a gateway.
    ///
    /// This part of an order is reversible in our own ledgers, which is why it
    /// does not prevent cancellation the way a captured card charge does.
    pub fn stored_value_total(&self) -> Result<Money> {
        Money::sum(
            self.tenders
                .iter()
                .filter(|tender| tender.kind.is_stored_value())
                .map(|tender| tender.amount),
            self.currency,
        )
    }

    /// Record a refund and advance the status.
    pub fn record_refund(&mut self, record: RefundRecord) -> Result<()> {
        self.currency_check(record.plan.total)?;
        let refundable = self.refundable_amount()?;
        if record.plan.total > refundable {
            return Err(Error::validation(format!(
                "cannot refund {}: only {refundable} remains refundable",
                record.plan.total
            )));
        }
        self.amount_refunded = self.amount_refunded.try_add(record.plan.total)?;
        self.refunds.push(record);
        let next = if self.amount_refunded == self.amount_captured && self.amount_captured.is_positive()
        {
            OrderStatus::Refunded
        } else {
            OrderStatus::PartiallyRefunded
        };
        if self.status.can_transition_to(next) {
            self.status = next;
        }
        self.touch();
        Ok(())
    }

    /// How much of `tender` has already been refunded for `shop`.
    pub fn refunded_from_tender(&self, tender_index: usize, shop: &ShopId) -> Result<Money> {
        let mut total = Money::zero(self.currency);
        for record in &self.refunds {
            for tender in &record.plan.tenders {
                if tender.tender_index != tender_index {
                    continue;
                }
                if let Some(amount) = tender.shop_allocation.get(shop.as_str()) {
                    total = total.try_add(*amount)?;
                }
            }
        }
        Ok(total)
    }

    /// Net quantity of a line that has already been refunded.
    pub fn refunded_quantity(&self, line_id: &crate::ids::LineItemId) -> u32 {
        self.refunds
            .iter()
            .flat_map(|record| record.plan.lines.iter())
            .filter(|line| &line.line_id == line_id)
            .map(|line| line.quantity)
            .sum()
    }

    /// Update a shipment's status, enforcing the fulfilment state machine.
    pub fn set_fulfillment_status(
        &mut self,
        group_id: &FulfillmentGroupId,
        next: FulfillmentStatus,
    ) -> Result<()> {
        let group = self
            .fulfillment_groups
            .iter_mut()
            .find(|group| &group.id == group_id)
            .ok_or_else(|| Error::not_found(FulfillmentGroupId::kind(), group_id))?;
        if group.status == next {
            return Ok(());
        }
        if !group.status.can_transition_to(next) {
            return Err(Error::InvalidTransition {
                kind: "fulfillment group",
                id: group_id.to_string(),
                from: format!("{:?}", group.status),
                to: format!("{next:?}"),
            });
        }
        group.status = next;
        self.touch();
        Ok(())
    }

    /// The shopper-paid amount attributable to one shipment.
    ///
    /// This is what you capture when a delivery completes and the merchant
    /// only takes money for goods that actually arrived.
    pub fn amount_for_fulfillment_group(&self, group_id: &FulfillmentGroupId) -> Result<Money> {
        let mut total = Money::zero(self.currency);
        for line in &self.quote.lines {
            if line.fulfillment_group_id.as_ref() == Some(group_id) {
                total = total.try_add(line.customer_total)?;
            }
        }
        for shipping in &self.quote.shipping {
            if &shipping.group_id == group_id {
                total = total.try_add(shipping.customer_total)?;
            }
        }
        Ok(total)
    }

    /// Whether every shipment has reached a delivered/terminal state.
    pub fn is_fully_fulfilled(&self) -> bool {
        !self.fulfillment_groups.is_empty()
            && self.fulfillment_groups.iter().all(|group| {
                matches!(group.status, FulfillmentStatus::Delivered)
                    || group.status.is_terminal()
            })
    }

    fn currency_check(&self, amount: Money) -> Result<()> {
        if amount.currency() == self.currency {
            Ok(())
        } else {
            Err(Error::CurrencyMismatch { expected: self.currency, actual: amount.currency() })
        }
    }

    fn touch(&mut self) {
        self.updated_at = Utc::now();
        self.version = self.version.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::CountryCode;
    use crate::cart::CartItem;
    use crate::payment::split::{PlatformFeePolicy, ShopAccounts};
    use crate::payment::tender::{TenderOffer, TenderPlan};
    use crate::pricing::{NoTaxCalculator, PricingEngine};
    use crate::gateway::{GatewayId, InstrumentRef};
    use std::sync::Arc;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    async fn order_with(items: &[(&str, i64)]) -> Order {
        let engine = PricingEngine::new(Arc::new(NoTaxCalculator));
        let mut cart = Cart::new(Currency::USD);
        cart.set_shipping_address(Address::new(CountryCode::US));
        for (shop, price) in items {
            cart.add_item(CartItem::new(*shop, "sku", "Item", usd(*price), 1).unwrap()).unwrap();
        }
        cart.regroup_fulfillment(|_| Ok(usd(0))).unwrap();

        let quote = engine.quote(&cart, &[]).await.unwrap();
        let settlement =
            SettlementPlan::from_quote(&quote, &ShopAccounts::new(), &PlatformFeePolicy::none())
                .unwrap();
        let tenders = TenderPlan::build(
            &quote,
            &[TenderOffer::gateway(
                GatewayId::from_static("mock"),
                InstrumentRef::SingleUseToken { token: "tok".into() },
                "visa •••• 4242",
            )],
        )
        .unwrap();
        Order::from_quote(&cart, quote, settlement, &tenders).unwrap()
    }

    #[tokio::test]
    async fn capture_progresses_the_status() {
        let mut order = order_with(&[("shop-1", 10_000)]).await;
        assert_eq!(order.status, OrderStatus::Draft);
        order.transition_to(OrderStatus::Authorized).unwrap();

        order.record_capture(usd(4_000)).unwrap();
        assert_eq!(order.status, OrderStatus::PartiallyCaptured);
        order.record_capture(usd(6_000)).unwrap();
        assert_eq!(order.status, OrderStatus::Paid);
        assert!(order.record_capture(usd(1)).is_err());
    }

    #[tokio::test]
    async fn illegal_transitions_are_rejected() {
        let mut order = order_with(&[("shop-1", 1_000)]).await;
        assert!(order.transition_to(OrderStatus::Fulfilled).is_err());
        order.transition_to(OrderStatus::Authorized).unwrap();
        order.transition_to(OrderStatus::Canceled).unwrap();
        assert!(order.status.is_terminal());
        assert!(order.transition_to(OrderStatus::Paid).is_err());
    }

    #[tokio::test]
    async fn per_shipment_amounts_support_capture_on_delivery() {
        let mut order = order_with(&[("shop-1", 6_000), ("shop-2", 4_000)]).await;
        assert_eq!(order.fulfillment_groups.len(), 2);

        let first = order.fulfillment_groups[0].id.clone();
        assert_eq!(order.amount_for_fulfillment_group(&first).unwrap(), usd(6_000));

        order.set_fulfillment_status(&first, FulfillmentStatus::Processing).unwrap();
        order.set_fulfillment_status(&first, FulfillmentStatus::Shipped).unwrap();
        order.set_fulfillment_status(&first, FulfillmentStatus::Delivered).unwrap();
        assert!(!order.is_fully_fulfilled());

        assert!(order.set_fulfillment_status(&first, FulfillmentStatus::Processing).is_err());
    }

    #[tokio::test]
    async fn tender_plan_must_match_the_order_total() {
        let engine = PricingEngine::new(Arc::new(NoTaxCalculator));
        let mut cart = Cart::new(Currency::USD);
        cart.set_shipping_address(Address::new(CountryCode::US));
        cart.add_item(CartItem::new("shop-1", "sku", "Item", usd(1_000), 1).unwrap()).unwrap();
        let quote = engine.quote(&cart, &[]).await.unwrap();
        let settlement =
            SettlementPlan::from_quote(&quote, &ShopAccounts::new(), &PlatformFeePolicy::none())
                .unwrap();
        let mut tenders = TenderPlan::build(
            &quote,
            &[TenderOffer::gateway(
                GatewayId::from_static("mock"),
                InstrumentRef::SingleUseToken { token: "tok".into() },
                "card",
            )],
        )
        .unwrap();
        tenders.total_due = usd(999);
        assert!(Order::from_quote(&cart, quote, settlement, &tenders).is_err());
    }
}
