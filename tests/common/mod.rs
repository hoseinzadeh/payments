//! Shared test harness for the scenario tests.
//!
//! Each integration test binary compiles this module separately, so items used
//! by only some of them would otherwise warn.
#![allow(dead_code)]

use std::sync::Arc;

use payments::address::{Address, CountryCode, Jurisdiction};
use payments::cart::service::FlatShippingRate;
use payments::gateway::CaptureMode;
use payments::prelude::*;

/// A fully wired engine plus the repositories behind it.
pub struct Harness {
    pub repositories: Repositories,
    pub checkout: CheckoutEngine,
    pub carts: CartService,
    pub gateway: Arc<MockGateway>,
}

impl Harness {
    /// Build a harness: 10 % California tax, 10 % platform commission,
    /// $5.99 flat shipping, automatic capture.
    pub fn new() -> Self {
        Self::with_capture_mode(CaptureMode::Automatic)
    }

    pub fn with_capture_mode(capture_mode: CaptureMode) -> Self {
        // A round 10 % keeps every expected figure in the tests exact and
        // readable, so a failure points at logic rather than at rounding.
        let tax = RateTableTaxCalculator::with_rules([TaxRule::new(
            "CA State Tax",
            "state",
            Jurisdiction::region(CountryCode::US, "CA"),
            1_000,
        )]);
        let pricing = PricingEngine::new(Arc::new(tax));

        let gateway = Arc::new(MockGateway::new());
        let gateways = GatewayRegistry::new().register(gateway.clone());

        let repositories = Repositories {
            carts: Arc::new(InMemoryCartRepository::new()),
            orders: Arc::new(InMemoryOrderRepository::new()),
            payments: Arc::new(InMemoryPaymentRepository::new()),
            discounts: Arc::new(InMemoryDiscountRepository::new()),
            gift_cards: Arc::new(InMemoryGiftCardRepository::new()),
            shop_credit: Arc::new(InMemoryShopCreditRepository::new()),
        };

        let config = CheckoutConfig {
            platform_fee: PlatformFeePolicy::percentage(1_000),
            accounts: ShopAccounts::new(),
            capture_mode,
            use_connected_accounts: false,
            statement_descriptor: Some("TESTSHOP".to_owned()),
        };

        let checkout = CheckoutEngine::new(pricing, gateways, repositories.clone())
            .with_config(config);
        let carts = CartService::with_shipping(
            repositories.carts.clone(),
            Arc::new(FlatShippingRate(usd(0))),
        );

        Self { repositories, checkout, carts, gateway }
    }

    /// Create a cart for `customer` containing `(shop, sku, unit price, qty)`.
    pub async fn cart_with(
        &self,
        customer: Option<CustomerId>,
        items: &[(&str, &str, i64, u32)],
    ) -> Cart {
        let cart = self.carts.create(customer, Currency::USD).await.expect("create cart");
        for (shop, sku, price, quantity) in items {
            self.carts
                .add_item(
                    &cart.id,
                    CartItem::new(*shop, *sku, *sku, usd(*price), *quantity).expect("item"),
                )
                .await
                .expect("add item");
        }
        self.carts
            .set_shipping_address(&cart.id, Address::new(CountryCode::US).with_region("CA"))
            .await
            .expect("address")
    }

    pub async fn register_discount(&self, discount: Discount) {
        self.repositories.discounts.save(&discount).await.expect("save discount");
    }

    pub async fn issue_gift_card(&self, code: &str, amount: i64) -> GiftCard {
        let card = GiftCard::issue(code, usd(amount)).expect("issue gift card");
        self.repositories.gift_cards.save(&card).await.expect("save gift card");
        card
    }

    pub async fn grant_credit(&self, customer: &CustomerId, shop: &str, amount: i64) {
        let mut account = ShopCreditAccount::new(
            customer.clone(),
            ShopId::from_string(shop),
            Currency::USD,
        );
        account.grant(usd(amount), "test grant", None).expect("grant");
        self.repositories.shop_credit.save(&account).await.expect("save credit");
    }

    pub async fn credit_balance(&self, customer: &CustomerId, shop: &str) -> Money {
        self.repositories
            .shop_credit
            .get(customer, &ShopId::from_string(shop), Currency::USD)
            .await
            .expect("load credit")
            .map(|account| account.balance())
            .unwrap_or(usd(0))
    }

    pub async fn gift_card_balance(&self, id: &GiftCardId) -> Money {
        self.repositories
            .gift_cards
            .get(id)
            .await
            .expect("load gift card")
            .map(|card| card.balance)
            .unwrap_or(usd(0))
    }

    /// A card tender that always succeeds.
    pub fn card() -> TenderOffer {
        TenderOffer::gateway(
            GatewayId::from_static("mock"),
            single_use("tok_visa_4242"),
            "visa •••• 4242",
        )
    }
}

pub fn usd(minor: i64) -> Money {
    Money::from_minor(minor, Currency::USD)
}

/// Assert that a settlement plan, its quote and its refunds all balance.
pub fn assert_order_balances(order: &Order) {
    order.quote.verify().expect("quote balances");
    order.settlement.verify().expect("settlement balances");
    for record in &order.refunds {
        record.plan.verify().expect("refund balances");
    }

    let shop_gross = Money::sum(
        order.settlement.shops.iter().map(|shop| shop.gross),
        order.currency,
    )
    .expect("sum");
    let funded = order
        .settlement
        .collected_from_customer
        .try_add(order.settlement.collected_from_funders)
        .expect("add");
    assert_eq!(shop_gross, funded, "shops must be owed exactly what was collected");
}
