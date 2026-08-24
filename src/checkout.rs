//! The checkout orchestrator.
//!
//! [`CheckoutEngine`] is the one type most applications interact with. It wires
//! pricing, fraud screening, split settlement, multi-tender funding, gateway
//! routing and persistence into a small number of operations:
//!
//! ```text
//! quote()    -> live totals for the checkout page
//! checkout() -> place the order, redeem stored value, authorise the gateways
//! confirm()  -> merchant approval to take the held funds
//! capture()  -> take funds, per order or per shipment
//! refund()   -> return funds to every instrument, shop and funder involved
//! cancel()   -> release holds and give stored value back
//! ```
//!
//! Every operation that moves money is idempotent given the same key, and every
//! failure path unwinds the stored-value redemptions it made, so a declined
//! card never leaves a gift card silently drained.

use chrono::Utc;
use std::sync::Arc;

use crate::cart::Cart;
use crate::error::{Error, Result};
use crate::fraud::{AllowAllFraudEngine, FraudEngine, RiskAssessment, RiskContext, RiskDecision};
use crate::gateway::{
    AuthorizeRequest, CancelRequest, CaptureMode, CaptureRequest, GatewayRefundRequest,
    GatewayRegistry, InstrumentRef, RoutingContext, TransferInstruction,
};
use crate::ids::{CartId, FulfillmentGroupId, OrderId, ShopId};
use crate::money::Money;
use crate::order::{Order, OrderStatus};
use crate::payment::method::ChargeInitiator;
use crate::payment::refund::{RefundPlan, RefundRecord, RefundRequest};
use crate::payment::split::{PlatformFeePolicy, SettlementPlan, ShopAccounts};
use crate::payment::tender::{TenderKind, TenderOffer, TenderPlan};
use crate::payment::{Payment, PaymentStatus};
use crate::pricing::{Discount, PricingEngine, Quote};
use crate::storage::{
    CartRepository, DiscountRepository, GiftCardRepository, OrderRepository, PaymentRepository,
    ShopCreditRepository,
};

/// The repositories the engine needs.
#[derive(Clone)]
pub struct Repositories {
    /// Carts.
    pub carts: Arc<dyn CartRepository>,
    /// Orders.
    pub orders: Arc<dyn OrderRepository>,
    /// Payments.
    pub payments: Arc<dyn PaymentRepository>,
    /// Promotions.
    pub discounts: Arc<dyn DiscountRepository>,
    /// Gift cards.
    pub gift_cards: Arc<dyn GiftCardRepository>,
    /// Shop credit.
    pub shop_credit: Arc<dyn ShopCreditRepository>,
}

/// Engine-wide policy.
#[derive(Clone)]
pub struct CheckoutConfig {
    /// Commission taken from each shop.
    pub platform_fee: PlatformFeePolicy,
    /// Where each shop's money is sent.
    pub accounts: ShopAccounts,
    /// Whether to capture immediately or hold funds until fulfilment.
    ///
    /// Delivery businesses should use [`CaptureMode::Manual`]: authorise at
    /// checkout, capture when the goods arrive.
    pub capture_mode: CaptureMode,
    /// Whether to route splits to connected accounts at the gateway.
    pub use_connected_accounts: bool,
    /// Text shown on the shopper's statement.
    pub statement_descriptor: Option<String>,
}

impl Default for CheckoutConfig {
    fn default() -> Self {
        Self {
            platform_fee: PlatformFeePolicy::none(),
            accounts: ShopAccounts::new(),
            capture_mode: CaptureMode::Automatic,
            use_connected_accounts: false,
            statement_descriptor: None,
        }
    }
}

/// What the caller asks for at checkout.
#[derive(Debug, Clone)]
pub struct CheckoutRequest {
    /// Cart to convert into an order.
    pub cart_id: CartId,
    /// Gateway instruments the shopper chose. Stored value staged on the cart
    /// (gift cards, shop credit) is added automatically and applied first.
    pub tenders: Vec<TenderOffer>,
    /// Makes retries safe. Reuse the same key when retrying a failed request.
    pub idempotency_key: String,
    /// Override the engine's capture mode for this order.
    pub capture_mode: Option<CaptureMode>,
    /// Who initiated the charge.
    pub initiator: ChargeInitiator,
    /// Signals for the fraud engine that only the caller knows.
    pub risk: RiskInputs,
}

impl CheckoutRequest {
    /// A checkout with one gateway instrument.
    pub fn new(cart_id: CartId, idempotency_key: impl Into<String>) -> Self {
        Self {
            cart_id,
            tenders: Vec::new(),
            idempotency_key: idempotency_key.into(),
            capture_mode: None,
            initiator: ChargeInitiator::CustomerOnSession,
            risk: RiskInputs::default(),
        }
    }

    /// Builder: add a tender.
    pub fn with_tender(mut self, tender: TenderOffer) -> Self {
        self.tenders.push(tender);
        self
    }

    /// Builder: hold funds instead of capturing them.
    pub fn holding_funds(mut self) -> Self {
        self.capture_mode = Some(CaptureMode::Manual);
        self
    }
}

/// Caller-supplied risk signals.
#[derive(Debug, Clone, Default)]
pub struct RiskInputs {
    /// Successful orders this shopper has completed before.
    pub prior_successful_orders: u32,
    /// Payment attempts in the last hour.
    pub attempts_last_hour: u32,
    /// Distinct instruments tried in the last hour.
    pub distinct_instruments_last_hour: u32,
    /// Whether the shopper's email is verified.
    pub email_verified: bool,
    /// Country of the client IP.
    pub ip_country: Option<crate::address::CountryCode>,
    /// Country the card was issued in.
    pub card_country: Option<crate::address::CountryCode>,
}

/// The outcome of a checkout.
#[derive(Debug, Clone)]
pub struct CheckoutResult {
    /// The placed order.
    pub order: Order,
    /// Payments created, one per gateway tender.
    pub payments: Vec<Payment>,
    /// The risk assessment that allowed the charge.
    pub risk: RiskAssessment,
    /// Shopper action required to finish (3-D Secure, redirect).
    pub next_action: Option<crate::gateway::NextAction>,
}

impl CheckoutResult {
    /// Whether the shopper still has to do something.
    pub fn requires_action(&self) -> bool {
        self.next_action.is_some()
    }
}

/// Orchestrates the whole payment lifecycle.
#[derive(Clone)]
pub struct CheckoutEngine {
    pricing: PricingEngine,
    gateways: GatewayRegistry,
    fraud: Arc<dyn FraudEngine>,
    repositories: Repositories,
    config: CheckoutConfig,
}

impl CheckoutEngine {
    /// Build an engine with the default (allow-all) fraud policy.
    pub fn new(
        pricing: PricingEngine,
        gateways: GatewayRegistry,
        repositories: Repositories,
    ) -> Self {
        Self {
            pricing,
            gateways,
            fraud: Arc::new(AllowAllFraudEngine),
            repositories,
            config: CheckoutConfig::default(),
        }
    }

    /// Builder: set the engine policy.
    pub fn with_config(mut self, config: CheckoutConfig) -> Self {
        self.config = config;
        self
    }

    /// Builder: plug in a fraud engine.
    pub fn with_fraud_engine(mut self, fraud: Arc<dyn FraudEngine>) -> Self {
        self.fraud = fraud;
        self
    }

    /// The configuration in use.
    pub fn config(&self) -> &CheckoutConfig {
        &self.config
    }

    /// Price a cart, resolving its promotion codes.
    ///
    /// Cheap enough to call on every change to the cart, which is what powers
    /// live totals during checkout.
    pub async fn quote(&self, cart: &Cart) -> Result<Quote> {
        let discounts = self.resolve_discounts(cart).await?;
        self.pricing.quote(cart, &discounts).await
    }

    /// Price a cart by id.
    pub async fn quote_cart(&self, cart_id: &CartId) -> Result<Quote> {
        let cart = self.load_cart(cart_id).await?;
        self.quote(&cart).await
    }

    /// Build the tender offers implied by a cart: gift cards staged on it and
    /// the shopper's shop credit, in the order they should be applied.
    pub async fn stored_value_offers(&self, cart: &Cart, quote: &Quote) -> Result<Vec<TenderOffer>> {
        let mut offers = Vec::new();
        let now = Utc::now();

        for code in &cart.gift_card_codes {
            let Some(card) = self.repositories.gift_cards.find_by_code(code).await? else {
                continue;
            };
            if !card.is_redeemable(now) || card.currency != cart.currency {
                continue;
            }
            let mut offer =
                TenderOffer::gift_card(card.id.clone(), card.masked_code.clone(), card.balance);
            if let Some(shop_id) = card.shop_id.clone() {
                offer = offer.restricted_to(shop_id);
            }
            offers.push(offer);
        }

        if cart.apply_shop_credit
            && let Some(customer_id) = &cart.customer_id
        {
            for totals in &quote.shop_totals {
                let account = self
                    .repositories
                    .shop_credit
                    .get(customer_id, &totals.shop_id, cart.currency)
                    .await?;
                if let Some(account) = account
                    && account.balance().is_positive()
                {
                    offers
                        .push(TenderOffer::shop_credit(totals.shop_id.clone(), account.balance()));
                }
            }
        }

        Ok(offers)
    }

    /// Place an order: price, screen, fund and authorise.
    pub async fn checkout(&self, request: &CheckoutRequest) -> Result<CheckoutResult> {
        let cart = self.load_cart(&request.cart_id).await?;
        cart.validate()?;

        let quote = self.quote(&cart).await?;
        if !quote.totals.total.is_positive() && quote.totals.subtotal.is_positive() {
            tracing::debug!("order is fully covered by discounts");
        }

        // 1. Screen before any money moves.
        let context = RiskContext {
            quote: Some(&quote),
            customer_id: cart.customer_id.clone(),
            card_country: request.risk.card_country,
            shipping_country: cart.shipping_address.as_ref().map(|address| address.country),
            billing_country: cart.billing_address.as_ref().map(|address| address.country),
            prior_successful_orders: request.risk.prior_successful_orders,
            attempts_last_hour: request.risk.attempts_last_hour,
            distinct_instruments_last_hour: request.risk.distinct_instruments_last_hour,
            email_verified: request.risk.email_verified,
            ip_country: request.risk.ip_country,
        };
        let risk = self.fraud.assess(&context).await?;
        if risk.decision == RiskDecision::Block {
            return Err(Error::Declined {
                code: crate::error::DeclineCode::Fraudulent,
                message: risk.summary(),
            });
        }

        // 2. Decide how the total is funded.
        let mut offers = self.stored_value_offers(&cart, &quote).await?;
        offers.extend(request.tenders.iter().cloned());
        let tender_plan = TenderPlan::build(&quote, &offers)?;

        // 3. Decide who gets paid.
        let settlement =
            SettlementPlan::from_quote(&quote, &self.config.accounts, &self.config.platform_fee)?;

        // 4. Create the order.
        let mut order = Order::from_quote(&cart, quote, settlement, &tender_plan)?;
        order.transition_to(OrderStatus::PendingPayment)?;
        self.repositories.orders.save(&order).await?;

        // 5. Spend stored value. Recorded first because it cannot fail
        //    asynchronously, and it is unwound if the gateway declines.
        if let Err(error) = self.redeem_stored_value(&order).await {
            self.release_stored_value(&order).await.ok();
            order.transition_to(OrderStatus::Failed)?;
            self.repositories.orders.save(&order).await?;
            return Err(error);
        }

        // 6. Authorise the gateway tenders.
        let capture_mode = request.capture_mode.unwrap_or(self.config.capture_mode);
        let mut payments = Vec::new();
        let mut next_action = None;

        for (index, tender) in tender_plan.tenders.iter().enumerate() {
            let TenderKind::Gateway { gateway, instrument, label, payment_method_id } = &tender.kind
            else {
                continue;
            };

            let routing = RoutingContext {
                currency: Some(order.currency),
                country: cart.billing_address.as_ref().map(|address| address.country),
                required_capabilities: if capture_mode == CaptureMode::Manual {
                    vec!["delayed_capture"]
                } else {
                    Vec::new()
                },
                preferred: Some(gateway.clone()),
            };
            let adapter = self.gateways.route(&routing)?;

            let mut payment = Payment::new(
                order.id.clone(),
                adapter.id(),
                tender.amount,
                format!("{}:tender:{index}", request.idempotency_key),
                label.clone(),
            );
            payment.tender_index = Some(index);
            payment.shop_allocation = tender.shop_allocation.clone();
            if let Some(id) = payment_method_id {
                payment.metadata.insert("payment_method_id".to_owned(), id.to_string());
            }

            let mut authorize = AuthorizeRequest::new(
                payment.idempotency_key.clone(),
                tender.amount,
                instrument.clone(),
            )
            .for_order(order.id.clone());
            authorize.customer_id = cart.customer_id.clone();
            authorize.capture_mode = capture_mode;
            authorize.initiator = request.initiator;
            authorize.statement_descriptor = self.config.statement_descriptor.clone();
            authorize.description = Some(format!("Order {}", order.id));
            if self.config.use_connected_accounts {
                authorize.transfers = self.transfers_for(&order, tender.amount)?;
                authorize.application_fee = Some(self.fee_for(&order, tender.amount)?);
            }
            authorize.validate()?;

            match adapter.authorize(&authorize).await {
                Ok(response) => {
                    if next_action.is_none() {
                        next_action = response.next_action.clone();
                    }
                    payment.record_authorization(&response)?;
                    self.repositories.payments.save(&payment).await?;
                    order.attach_payment(payment.id.clone());
                    payments.push(payment);
                }
                Err(error) => {
                    payment.record_failure(error.to_string());
                    self.repositories.payments.save(&payment).await.ok();
                    // Unwind everything that already succeeded.
                    self.void_payments(&payments).await;
                    self.release_stored_value(&order).await.ok();
                    order.transition_to(OrderStatus::Failed)?;
                    self.repositories.orders.save(&order).await?;
                    return Err(error);
                }
            }
        }

        // 7. Reflect the payment outcome on the order.
        let captured = Money::sum(
            payments.iter().map(|payment| payment.amount_captured),
            order.currency,
        )?;
        let stored_value = Money::sum(
            order.tenders.iter().filter(|t| t.kind.is_stored_value()).map(|t| t.amount),
            order.currency,
        )?;
        let collected = captured.try_add(stored_value)?;

        if payments.iter().any(|payment| payment.status == PaymentStatus::RequiresAction) {
            // Stay in PendingPayment until the shopper finishes the challenge.
        } else if collected.is_positive() {
            order.record_capture(collected)?;
        } else {
            order.transition_to(OrderStatus::Authorized)?;
        }
        self.repositories.orders.save(&order).await?;

        Ok(CheckoutResult { order, payments, risk, next_action })
    }

    /// Merchant approval to capture the funds held for an order.
    pub async fn confirm(&self, order_id: &OrderId) -> Result<Vec<Payment>> {
        let payments = self.repositories.payments.list_for_order(order_id).await?;
        let mut confirmed = Vec::new();
        for mut payment in payments {
            if payment.status.is_capturable() {
                payment.confirm()?;
                self.repositories.payments.save(&payment).await?;
            }
            confirmed.push(payment);
        }
        Ok(confirmed)
    }

    /// Capture a specific amount, prorated across the order's payments.
    pub async fn capture(&self, order_id: &OrderId, amount: Money) -> Result<Order> {
        let mut order = self.load_order(order_id).await?;
        let payments = self.repositories.payments.list_for_order(order_id).await?;

        let capturable: Vec<Money> = payments
            .iter()
            .map(|payment| payment.capturable_amount().unwrap_or_else(|_| Money::zero(order.currency)))
            .collect();
        let available = Money::sum(capturable.iter().copied(), order.currency)?;
        if amount > available {
            return Err(Error::validation(format!(
                "cannot capture {amount}: only {available} is held across this order's payments"
            )));
        }

        let shares = crate::money::allocate_by_weights(amount, &capturable)?;
        let mut total = Money::zero(order.currency);
        for (mut payment, share) in payments.into_iter().zip(shares) {
            if !share.is_positive() {
                continue;
            }
            let adapter = self.gateways.get(&payment.gateway)?;
            payment.check_capture(share, Utc::now())?;
            let transaction_id = payment
                .transaction_id
                .clone()
                .ok_or_else(|| Error::internal("payment has no transaction id"))?;
            let response = adapter
                .capture(&CaptureRequest {
                    idempotency_key: format!("{}:capture:{}", payment.id, payment.captures.len()),
                    transaction_id,
                    amount: share,
                    final_capture: false,
                })
                .await?;
            payment.record_capture(&response, None)?;
            self.repositories.payments.save(&payment).await?;
            total = total.try_add(share)?;
        }

        if total.is_positive() {
            order.record_capture(total)?;
            self.repositories.orders.save(&order).await?;
        }
        Ok(order)
    }

    /// Capture the amount belonging to one shipment.
    ///
    /// This is the delivery workflow: authorise the whole basket at checkout,
    /// then take money for each shipment as it is delivered.
    pub async fn capture_fulfillment_group(
        &self,
        order_id: &OrderId,
        group_id: &FulfillmentGroupId,
    ) -> Result<Order> {
        let order = self.load_order(order_id).await?;
        let amount = order.amount_for_fulfillment_group(group_id)?;
        if !amount.is_positive() {
            return Err(Error::validation("this shipment has no capturable amount"));
        }
        self.capture(order_id, amount).await
    }

    /// Release every hold on an order and give stored value back.
    ///
    /// Only possible while no gateway has actually taken money. Gift cards and
    /// shop credit are reversible in our own ledgers, so they do not block a
    /// cancellation the way a captured card charge does.
    pub async fn cancel(&self, order_id: &OrderId) -> Result<Order> {
        let mut order = self.load_order(order_id).await?;
        let payments = self.repositories.payments.list_for_order(order_id).await?;

        let gateway_captured =
            Money::sum(payments.iter().map(|payment| payment.amount_captured), order.currency)?;
        if gateway_captured.is_positive() {
            return Err(Error::validation(
                "this order has captured funds; refund it instead of cancelling",
            ));
        }

        for mut payment in payments {
            let Some(transaction_id) = payment.transaction_id.clone() else {
                continue;
            };
            if !payment.status.is_capturable() && payment.status != PaymentStatus::RequiresAction {
                continue;
            }
            let adapter = self.gateways.get(&payment.gateway)?;
            adapter
                .cancel(&CancelRequest {
                    idempotency_key: format!("{}:cancel", payment.id),
                    transaction_id,
                    reason: Some("order canceled".to_owned()),
                })
                .await?;
            payment.record_cancellation()?;
            self.repositories.payments.save(&payment).await?;
        }

        self.release_stored_value(&order).await?;
        // The stored value was recorded as collected at checkout; it has just
        // been handed back, so the order no longer holds any of it.
        let stored_value = order.stored_value_total()?;
        if stored_value.is_positive() {
            let reversible = stored_value.try_min(order.amount_captured)?;
            order.reverse_capture(reversible)?;
        }
        order.transition_to(OrderStatus::Canceled)?;
        self.repositories.orders.save(&order).await?;
        Ok(order)
    }

    /// Refund an order, returning money to every instrument that paid.
    pub async fn refund(&self, order_id: &OrderId, request: &RefundRequest) -> Result<Order> {
        let mut order = self.load_order(order_id).await?;

        if order
            .refunds
            .iter()
            .any(|record| record.idempotency_key == request.idempotency_key)
        {
            return Ok(order);
        }

        let plan = RefundPlan::build(&order, request)?;
        let payments = self.repositories.payments.list_for_order(order_id).await?;
        let mut gateway_references = Vec::new();

        for tender_refund in &plan.tenders {
            match &tender_refund.kind {
                TenderKind::Gateway { .. } => {
                    let mut payment = payments
                        .iter()
                        .find(|payment| payment.tender_index == Some(tender_refund.tender_index))
                        .cloned()
                        .ok_or_else(|| {
                            Error::internal(format!(
                                "no payment found for tender {}",
                                tender_refund.tender_index
                            ))
                        })?;
                    let transaction_id = payment
                        .transaction_id
                        .clone()
                        .ok_or_else(|| Error::internal("payment has no transaction id"))?;
                    let adapter = self.gateways.get(&payment.gateway)?;
                    let response = adapter
                        .refund(&GatewayRefundRequest {
                            idempotency_key: format!(
                                "{}:{}",
                                request.idempotency_key, tender_refund.tender_index
                            ),
                            transaction_id,
                            amount: tender_refund.amount,
                            reason: request.reason,
                            reverse_transfers: self.config.use_connected_accounts,
                            refund_application_fee: request.refund_platform_fee,
                            metadata: Default::default(),
                        })
                        .await?;
                    payment.record_refund(tender_refund.amount)?;
                    self.repositories.payments.save(&payment).await?;
                    gateway_references.push(response.refund_reference);
                }
                TenderKind::GiftCard { gift_card_id, .. } => {
                    let mut card = self
                        .repositories
                        .gift_cards
                        .get(gift_card_id)
                        .await?
                        .ok_or_else(|| Error::not_found("gift card", gift_card_id))?;
                    card.restore(tender_refund.amount, order.id.clone(), Utc::now())?;
                    self.repositories.gift_cards.save(&card).await?;
                }
                TenderKind::ShopCredit { shop_id } => {
                    let customer_id = order
                        .customer_id
                        .clone()
                        .ok_or_else(|| Error::internal("shop credit without a customer"))?;
                    let mut account = self
                        .repositories
                        .shop_credit
                        .get(&customer_id, shop_id, order.currency)
                        .await?
                        .ok_or_else(|| Error::not_found("shop credit", shop_id))?;
                    account.reverse(
                        tender_refund.amount,
                        order.id.clone(),
                        Some(format!("{}:credit:{shop_id}", request.idempotency_key)),
                    )?;
                    self.repositories.shop_credit.save(&account).await?;
                }
            }
        }

        let mut record = RefundRecord::new(plan, request);
        record.gateway_references = gateway_references;
        order.record_refund(record)?;
        self.repositories.orders.save(&order).await?;
        Ok(order)
    }

    /// Load an order or fail.
    pub async fn load_order(&self, id: &OrderId) -> Result<Order> {
        self.repositories
            .orders
            .get(id)
            .await?
            .ok_or_else(|| Error::not_found(OrderId::kind(), id))
    }

    async fn load_cart(&self, id: &CartId) -> Result<Cart> {
        self.repositories
            .carts
            .get(id)
            .await?
            .ok_or_else(|| Error::not_found(CartId::kind(), id))
    }

    async fn resolve_discounts(&self, cart: &Cart) -> Result<Vec<Discount>> {
        let mut discounts = self.repositories.discounts.list_automatic().await?;
        for code in &cart.discount_codes {
            if let Some(discount) = self.repositories.discounts.find_by_code(code).await? {
                discounts.push(discount);
            }
        }
        Ok(discounts)
    }

    async fn redeem_stored_value(&self, order: &Order) -> Result<()> {
        let now = Utc::now();
        for (index, tender) in order.tenders.iter().enumerate() {
            match &tender.kind {
                TenderKind::GiftCard { gift_card_id, .. } => {
                    let mut card = self
                        .repositories
                        .gift_cards
                        .get(gift_card_id)
                        .await?
                        .ok_or_else(|| Error::not_found("gift card", gift_card_id))?;
                    let key = format!("{}:tender:{index}", order.id);
                    if card.redemptions.iter().any(|r| r.idempotency_key.as_deref() == Some(&key)) {
                        continue;
                    }
                    card.redeem(tender.amount, order.id.clone(), now, Some(key))?;
                    self.repositories.gift_cards.save(&card).await?;
                }
                TenderKind::ShopCredit { shop_id } => {
                    let customer_id = order
                        .customer_id
                        .clone()
                        .ok_or_else(|| Error::internal("shop credit without a customer"))?;
                    let mut account = self
                        .repositories
                        .shop_credit
                        .get(&customer_id, shop_id, order.currency)
                        .await?
                        .ok_or_else(|| Error::not_found("shop credit", shop_id))?;
                    let key = format!("{}:tender:{index}", order.id);
                    if account.has_key(&key) {
                        continue;
                    }
                    account.redeem(tender.amount, order.id.clone(), Some(key))?;
                    self.repositories.shop_credit.save(&account).await?;
                }
                TenderKind::Gateway { .. } => {}
            }
        }
        Ok(())
    }

    async fn release_stored_value(&self, order: &Order) -> Result<()> {
        let now = Utc::now();
        for tender in &order.tenders {
            match &tender.kind {
                TenderKind::GiftCard { gift_card_id, .. } => {
                    if let Some(mut card) = self.repositories.gift_cards.get(gift_card_id).await? {
                        let spent = card.redeemed_for(&order.id)?;
                        if spent.is_positive() {
                            card.restore(spent, order.id.clone(), now)?;
                            self.repositories.gift_cards.save(&card).await?;
                        }
                    }
                }
                TenderKind::ShopCredit { shop_id } => {
                    let Some(customer_id) = order.customer_id.clone() else {
                        continue;
                    };
                    if let Some(mut account) = self
                        .repositories
                        .shop_credit
                        .get(&customer_id, shop_id, order.currency)
                        .await?
                    {
                        let spent = account.redeemed_for(&order.id)?;
                        if spent.is_positive() {
                            account.reverse(spent, order.id.clone(), None)?;
                            self.repositories.shop_credit.save(&account).await?;
                        }
                    }
                }
                TenderKind::Gateway { .. } => {}
            }
        }
        Ok(())
    }

    async fn void_payments(&self, payments: &[Payment]) {
        for payment in payments {
            let Some(transaction_id) = payment.transaction_id.clone() else {
                continue;
            };
            let Ok(adapter) = self.gateways.get(&payment.gateway) else {
                continue;
            };
            let _ = adapter
                .cancel(&CancelRequest {
                    idempotency_key: format!("{}:rollback", payment.id),
                    transaction_id,
                    reason: Some("checkout failed".to_owned()),
                })
                .await;
        }
    }

    /// Split one gateway charge across the connected accounts of the shops it
    /// funds, proportionally to what each shop is owed from that tender.
    fn transfers_for(&self, order: &Order, amount: Money) -> Result<Vec<TransferInstruction>> {
        let mut transfers = Vec::new();
        for shop in &order.settlement.shops {
            let share = self.shop_share(order, &shop.shop_id, amount)?;
            let net = share.try_sub(self.fee_share(order, &shop.shop_id, amount)?)?;
            if net.is_positive() {
                transfers.push(TransferInstruction {
                    destination: shop.account_id.clone(),
                    amount: net,
                    description: Some(format!("Order {} — {}", order.id, shop.shop_id)),
                    transfer_group: Some(order.id.to_string()),
                });
            }
        }
        Ok(transfers)
    }

    fn fee_for(&self, order: &Order, amount: Money) -> Result<Money> {
        let mut total = Money::zero(order.currency);
        for shop in &order.settlement.shops {
            total = total.try_add(self.fee_share(order, &shop.shop_id, amount)?)?;
        }
        Ok(total)
    }

    fn shop_share(&self, order: &Order, shop_id: &ShopId, amount: Money) -> Result<Money> {
        let weights: Vec<Money> =
            order.settlement.shops.iter().map(|shop| shop.funded_by_customer).collect();
        let shares = crate::money::allocate_by_weights(amount, &weights)?;
        Ok(order
            .settlement
            .shops
            .iter()
            .zip(shares)
            .find(|(shop, _)| &shop.shop_id == shop_id)
            .map(|(_, share)| share)
            .unwrap_or_else(|| Money::zero(order.currency)))
    }

    fn fee_share(&self, order: &Order, shop_id: &ShopId, amount: Money) -> Result<Money> {
        let Some(settlement) = order.settlement.shop(shop_id) else {
            return Ok(Money::zero(order.currency));
        };
        if !settlement.gross.is_positive() || !settlement.platform_fee.is_positive() {
            return Ok(Money::zero(order.currency));
        }
        let share = self.shop_share(order, shop_id, amount)?;
        settlement.platform_fee.mul_ratio(
            share.minor(),
            settlement.funded_by_customer.minor().max(1),
            crate::money::Rounding::HalfUp,
        )
    }
}

/// Convenience helper to build an [`InstrumentRef`] from a raw client token.
pub fn single_use(token: impl Into<String>) -> InstrumentRef {
    InstrumentRef::SingleUseToken { token: token.into() }
}

/// Reconciles orders and payments from verified gateway webhooks.
///
/// Asynchronous flows — a 3-D Secure challenge finishing, a bank debit
/// settling, an authorisation lapsing — complete out of band, and the webhook
/// is the only notification you get. Register this handler with a
/// [`WebhookProcessor`](crate::webhook::WebhookProcessor) and those flows land
/// on the order automatically.
///
/// The handler re-reads the transaction from the gateway rather than trusting
/// the amounts in the event body, so an out-of-order or replayed delivery still
/// converges on the provider's current truth.
#[derive(Clone)]
pub struct OrderWebhookHandler {
    gateways: GatewayRegistry,
    repositories: Repositories,
}

impl OrderWebhookHandler {
    /// Build a handler over the same registry and repositories as the engine.
    pub fn new(gateways: GatewayRegistry, repositories: Repositories) -> Self {
        Self { gateways, repositories }
    }
}

#[async_trait::async_trait]
impl crate::webhook::WebhookHandler for OrderWebhookHandler {
    async fn handle(&self, event: &crate::gateway::GatewayEvent) -> Result<()> {
        use crate::gateway::GatewayEventKind;

        let Some(transaction_id) = &event.transaction_id else {
            return Ok(());
        };
        let Some(mut payment) =
            self.repositories.payments.find_by_transaction(transaction_id).await?
        else {
            // The event belongs to a payment we do not know about. Acknowledge
            // it: retrying will not make it ours.
            tracing::debug!(%transaction_id, "webhook for an unknown transaction");
            return Ok(());
        };

        match &event.kind {
            GatewayEventKind::PaymentAuthorized | GatewayEventKind::PaymentCaptured => {
                let adapter = self.gateways.get(&payment.gateway)?;
                let state = adapter.fetch_transaction(transaction_id).await?;
                let before = payment.amount_captured;
                payment.record_authorization(&state)?;
                self.repositories.payments.save(&payment).await?;

                let delta = payment.amount_captured.try_sub(before)?;
                if delta.is_positive() {
                    let mut order = self
                        .repositories
                        .orders
                        .get(&payment.order_id)
                        .await?
                        .ok_or_else(|| Error::not_found(OrderId::kind(), &payment.order_id))?;
                    order.record_capture(delta)?;
                    self.repositories.orders.save(&order).await?;
                }
            }
            GatewayEventKind::PaymentFailed => {
                payment.record_failure("the gateway reported the payment as failed");
                self.repositories.payments.save(&payment).await?;
                if let Some(mut order) =
                    self.repositories.orders.get(&payment.order_id).await?
                    && order.transition_to(OrderStatus::Failed).is_ok()
                {
                    self.repositories.orders.save(&order).await?;
                }
            }
            GatewayEventKind::PaymentCanceled => {
                if payment.record_cancellation().is_ok() {
                    self.repositories.payments.save(&payment).await?;
                }
            }
            GatewayEventKind::AuthorizationExpired => {
                payment.record_expiry();
                self.repositories.payments.save(&payment).await?;
            }
            // Refunds and disputes are driven from our side or handled by a
            // dedicated handler; acknowledging keeps the provider from retrying.
            _ => {}
        }
        Ok(())
    }
}
