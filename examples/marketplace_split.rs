//! Several shops in one basket, a third-party subsidy, a platform fee, and
//! capture on delivery.
//!
//! Run with: `cargo run --example marketplace_split`

use std::sync::Arc;

use payments::address::{Address, CountryCode, Jurisdiction};
use payments::cart::FulfillmentMethod;
use payments::gateway::CaptureMode;
use payments::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let (repositories, checkout) = build_engine().await?;
    let carts = CartService::with_shipping(
        repositories.carts.clone(),
        Arc::new(payments::cart::service::FlatShippingRate(Money::from_minor(599, Currency::USD))),
    );

    // Items from three different shops, one of them delivered on a schedule.
    let customer = CustomerId::new();
    let cart = carts.create(Some(customer.clone()), Currency::USD).await?;
    for (shop, sku, name, price) in [
        ("bakery", "CAKE", "Birthday cake", 4_500),
        ("florist", "ROSES", "Dozen roses", 3_500),
        ("grocer", "FRUIT", "Fruit basket", 2_000),
    ] {
        carts
            .add_item(
                &cart.id,
                CartItem::new(shop, sku, name, Money::from_minor(price, Currency::USD), 1)?
                    .with_fulfillment(FulfillmentSelection::new(FulfillmentMethod::LocalDelivery {
                        window: Some("18:00-20:00".into()),
                    })),
            )
            .await?;
    }
    let cart = carts
        .set_shipping_address(&cart.id, Address::new(CountryCode::US).with_region("CA"))
        .await?;
    // A platform-funded promotion: the shopper pays less, the shops do not.
    let cart = carts.apply_discount_code(&cart.id, "WELCOME15").await?;

    let quote = checkout.quote(&cart).await?;
    println!("Subtotal          {}", quote.totals.subtotal);
    println!("Shipping          {}", quote.totals.shipping);
    println!("Platform subsidy  {}", quote.totals.subsidy_discount);
    println!("Tax               {}", quote.totals.tax);
    println!("Shopper pays      {}", quote.amount_due());
    println!("Shops are owed    {}", quote.totals.merchant_gross);
    println!("(the difference is billed to the funding account)\n");

    // Authorise now, capture per delivery.
    let request = CheckoutRequest::new(cart.id.clone(), "order-100")
        .with_tender(TenderOffer::gateway(
            GatewayId::from_static("mock"),
            single_use("tok_visa_4242"),
            "visa •••• 4242",
        ))
        .holding_funds();
    let result = checkout.checkout(&request).await?;
    let order_id = result.order.id.clone();
    println!("Order {order_id} is {:?} (funds held, not taken)", result.order.status);

    for shop in &result.order.settlement.shops {
        println!(
            "  {} gross {} fee {} net {} (customer {} / subsidy {})",
            shop.shop_id,
            shop.gross,
            shop.platform_fee,
            shop.net,
            shop.funded_by_customer,
            shop.funded_by_subsidy
        );
    }
    for funder in &result.order.settlement.funders {
        println!("  funder {} owes {}", funder.funder, funder.amount);
    }

    // The merchant approves capture, then each delivery captures its own share.
    checkout.confirm(&order_id).await?;
    let groups: Vec<_> = result.order.fulfillment_groups.iter().map(|g| g.id.clone()).collect();
    println!("\n{} shipments to deliver", groups.len());
    for group in &groups {
        let order = checkout.capture_fulfillment_group(&order_id, group).await?;
        println!("  delivered {group}: captured so far {}", order.amount_captured);
    }

    let order = checkout.load_order(&order_id).await?;
    println!("\nFinal status {:?}, captured {}", order.status, order.amount_captured);

    // Full refund: every shop, the funder and the card are all unwound.
    let order = checkout.refund(&order_id, &RefundRequest::full("refund-100")).await?;
    let record = order.refunds.last().expect("a refund was recorded");
    println!("\nRefunded to shopper  {}", record.plan.total);
    println!("Reclaimed from funder {}", record.plan.subsidy_reclaimed);
    println!("Order status          {:?}", order.status);

    let reporting = Reporting::new(repositories.orders.clone());
    for shop in reporting.by_shop(Currency::USD, DateRange::all_time()).await? {
        println!("  report {}: gross {} net {}", shop.shop_id, shop.gross, shop.net);
    }

    Ok(())
}

async fn build_engine() -> Result<(Repositories, CheckoutEngine)> {
    let tax = RateTableTaxCalculator::with_rules([TaxRule::new(
        "CA State Tax",
        "state",
        Jurisdiction::region(CountryCode::US, "CA"),
        725,
    )]);
    let pricing = PricingEngine::new(Arc::new(tax));
    let gateways = GatewayRegistry::new().register(Arc::new(MockGateway::new()));

    let repositories = Repositories {
        carts: Arc::new(InMemoryCartRepository::new()),
        orders: Arc::new(InMemoryOrderRepository::new()),
        payments: Arc::new(InMemoryPaymentRepository::new()),
        discounts: Arc::new(InMemoryDiscountRepository::new()),
        gift_cards: Arc::new(InMemoryGiftCardRepository::new()),
        shop_credit: Arc::new(InMemoryShopCreditRepository::new()),
    };

    repositories
        .discounts
        .save(
            &Discount::percentage_off("WELCOME15", "Welcome promotion (platform funded)", 1_500)
                .funded_by(AccountId::from_string("acct_platform"), "welcome"),
        )
        .await?;

    let config = CheckoutConfig {
        platform_fee: PlatformFeePolicy::percentage(1_000).plus_fixed(Money::from_minor(30, Currency::USD)),
        accounts: ShopAccounts::new()
            .with(ShopId::from_string("bakery"), AccountId::from_string("acct_bakery"))
            .with(ShopId::from_string("florist"), AccountId::from_string("acct_florist"))
            .with(ShopId::from_string("grocer"), AccountId::from_string("acct_grocer")),
        capture_mode: CaptureMode::Manual,
        use_connected_accounts: true,
        statement_descriptor: Some("MARKETPLACE".to_owned()),
    };

    let checkout = CheckoutEngine::new(pricing, gateways, repositories.clone())
        .with_config(config)
        .with_fraud_engine(Arc::new(RuleBasedFraudEngine::with_policy(FraudPolicy {
            high_value_threshold: Some(Money::from_minor(50_000, Currency::USD)),
            ..Default::default()
        })));

    Ok((repositories, checkout))
}
