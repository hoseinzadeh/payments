//! Single shop, no split: the simplest possible checkout.
//!
//! Run with: `cargo run --example single_shop`

use std::sync::Arc;

use payments::address::{Address, CountryCode, Jurisdiction};
use payments::prelude::*;
use payments::pricing::PricingConfig;

#[tokio::main]
async fn main() -> Result<()> {
    let engine = build_engine();
    let carts = CartService::new(engine.0.carts.clone());
    let checkout = engine.1;

    // 1. The shopper fills a cart.
    let customer = CustomerId::new();
    let cart = carts.create(Some(customer.clone()), Currency::USD).await?;
    let cart = carts
        .add_item(
            &cart.id,
            CartItem::new("acme", "TEE-01", "Cotton T-shirt", Money::parse_decimal("24.99", Currency::USD)?, 2)?,
        )
        .await?;
    let cart = carts
        .add_item(
            &cart.id,
            CartItem::new("acme", "MUG-01", "Enamel mug", Money::parse_decimal("12.50", Currency::USD)?, 1)?,
        )
        .await?;
    let cart = carts
        .set_shipping_address(&cart.id, Address::new(CountryCode::US).with_region("CA").with_postal_code("94107"))
        .await?;

    // 2. Live totals for the checkout page. Cheap enough to call on every edit.
    let quote = checkout.quote(&cart).await?;
    println!("Subtotal   {}", quote.totals.subtotal);
    println!("Shipping   {}", quote.totals.shipping);
    println!("Tax        {}", quote.totals.tax);
    println!("Total      {}", quote.amount_due());

    // 3. Pay with a card token produced by the gateway's client SDK.
    let request = CheckoutRequest::new(cart.id.clone(), "order-001").with_tender(TenderOffer::gateway(
        GatewayId::from_static("mock"),
        single_use("tok_visa_4242"),
        "visa •••• 4242",
    ));
    let result = checkout.checkout(&request).await?;

    println!("\nOrder {} is {:?}", result.order.id, result.order.status);
    println!("Captured   {}", result.order.amount_captured);
    println!("Risk       {}", result.risk.summary());

    // 4. The shop is owed the whole charge because there is no split and no fee.
    for shop in &result.order.settlement.shops {
        println!("Settle     {} -> {} ({})", shop.shop_id, shop.net, shop.account_id);
    }

    // 5. Errors are typed: a declined card is not a generic failure.
    let declined = CheckoutRequest::new(cart.id.clone(), "order-002").with_tender(
        TenderOffer::gateway(GatewayId::from_static("mock"), single_use("tok_decline"), "declined card"),
    );
    match checkout.checkout(&declined).await {
        Err(Error::Declined { code, .. }) => {
            println!("\nSecond attempt declined ({code}): {}", code.customer_message());
        }
        other => println!("\nUnexpected: {other:?}"),
    }

    Ok(())
}

fn build_engine() -> (Repositories, CheckoutEngine) {
    let tax = RateTableTaxCalculator::with_rules([
        TaxRule::new("CA State Tax", "state", Jurisdiction::region(CountryCode::US, "CA"), 725),
        TaxRule::new(
            "SF District Tax",
            "district",
            Jurisdiction { country: CountryCode::US, region: Some("CA".into()), postal_code: Some("94107".into()) },
            113,
        ),
    ]);
    let pricing = PricingEngine::with_config(Arc::new(tax), PricingConfig::default());
    let gateways = GatewayRegistry::new().register(Arc::new(MockGateway::new()));

    let repositories = Repositories {
        carts: Arc::new(InMemoryCartRepository::new()),
        orders: Arc::new(InMemoryOrderRepository::new()),
        payments: Arc::new(InMemoryPaymentRepository::new()),
        discounts: Arc::new(InMemoryDiscountRepository::new()),
        gift_cards: Arc::new(InMemoryGiftCardRepository::new()),
        shop_credit: Arc::new(InMemoryShopCreditRepository::new()),
    };
    let checkout = CheckoutEngine::new(pricing, gateways, repositories.clone());
    (repositories, checkout)
}
