//! Gift card + shop credit + card in one transaction, then a partial refund
//! that returns money to each instrument in the right proportion.
//!
//! Run with: `cargo run --example multi_tender_refund`

use std::sync::Arc;

use payments::address::{Address, CountryCode};
use payments::payment::refund::{RefundLineRequest, RefundRequest};
use payments::prelude::*;

#[tokio::main]
async fn main() -> Result<()> {
    let (repositories, checkout) = build_engine();
    let carts = CartService::new(repositories.carts.clone());
    let customer = CustomerId::new();

    // The shopper holds a 30.00 gift card and 10.00 of credit at one shop.
    let gift_card = GiftCard::issue("GIFT-2024-ABCD", Money::from_minor(3_000, Currency::USD))?;
    repositories.gift_cards.save(&gift_card).await?;

    let mut credit = ShopCreditAccount::new(
        customer.clone(),
        ShopId::from_string("bookstore"),
        Currency::USD,
    );
    credit.grant(Money::from_minor(1_000, Currency::USD), "loyalty reward", None)?;
    repositories.shop_credit.save(&credit).await?;

    // Two shops, 60.00 and 40.00.
    let cart = carts.create(Some(customer.clone()), Currency::USD).await?;
    carts
        .add_item(
            &cart.id,
            CartItem::new("bookstore", "BOOK", "Hardback", Money::from_minor(3_000, Currency::USD), 2)?,
        )
        .await?;
    carts
        .add_item(
            &cart.id,
            CartItem::new("record-shop", "LP", "Vinyl LP", Money::from_minor(4_000, Currency::USD), 1)?,
        )
        .await?;
    let mut cart = carts.set_shipping_address(&cart.id, Address::new(CountryCode::US)).await?;

    // Stage the stored value on the cart; the engine applies it before the card.
    cart.add_gift_card("GIFT-2024-ABCD");
    cart.set_apply_shop_credit(true);
    repositories.carts.save(&cart).await?;

    let quote = checkout.quote(&cart).await?;
    println!("Order total {}\n", quote.amount_due());

    let request = CheckoutRequest::new(cart.id.clone(), "order-200").with_tender(
        TenderOffer::gateway(
            GatewayId::from_static("mock"),
            single_use("tok_visa_4242"),
            "visa •••• 4242",
        ),
    );
    let result = checkout.checkout(&request).await?;
    let order_id = result.order.id.clone();

    println!("Funded by:");
    for tender in &result.order.tenders {
        println!("  {:<28} {}", tender.kind.label(), tender.amount);
    }

    let card_after = repositories.gift_cards.get(&gift_card.id).await?.expect("gift card");
    let credit_after = repositories
        .shop_credit
        .get(&customer, &ShopId::from_string("bookstore"), Currency::USD)
        .await?
        .expect("credit account");
    println!("\nGift card balance now {}", card_after.balance);
    println!("Shop credit balance now {}", credit_after.balance());

    // Return one book. Shop credit is bookstore-only, so it takes its share
    // back first, and the card gets the rest.
    let book_line = result
        .order
        .quote
        .lines
        .iter()
        .find(|line| line.sku == "BOOK")
        .expect("book line")
        .line_id
        .clone();

    let order = checkout
        .refund(
            &order_id,
            &RefundRequest::lines(
                "refund-200",
                vec![RefundLineRequest { line_id: book_line, quantity: 1 }],
            ),
        )
        .await?;

    let record = order.refunds.last().expect("refund recorded");
    println!("\nRefunded {} for one book:", record.plan.total);
    for tender in &record.plan.tenders {
        println!("  {:<28} {}", tender.kind.label(), tender.amount);
    }

    let card_after = repositories.gift_cards.get(&gift_card.id).await?.expect("gift card");
    let credit_after = repositories
        .shop_credit
        .get(&customer, &ShopId::from_string("bookstore"), Currency::USD)
        .await?
        .expect("credit account");
    println!("\nGift card balance now {}", card_after.balance);
    println!("Shop credit balance now {}", credit_after.balance());
    println!("Order status {:?}", order.status);

    Ok(())
}

fn build_engine() -> (Repositories, CheckoutEngine) {
    let pricing = PricingEngine::new(Arc::new(NoTaxCalculator));
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
