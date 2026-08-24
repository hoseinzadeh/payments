//! End-to-end tests for the six scenarios in `requirements.md`.
//!
//! Each test walks a complete money path and asserts on exact amounts, so the
//! figures below double as executable documentation of how the engine splits,
//! taxes and refunds an order.

mod common;

use common::{Harness, assert_order_balances, usd};

use payments::gateway::CaptureMode;
use payments::ids::AccountId;
use payments::payment::refund::{RefundLineRequest, RefundRequest};
use payments::prelude::*;

/// ### Scenario 1 — single shop, no payment split
///
/// $100 of goods + 10 % tax = $110 collected, all of it owed to one shop
/// (less the platform's 10 % commission on the goods).
#[tokio::test]
async fn single_shop_without_payment_split() {
    let harness = Harness::new();
    let customer = CustomerId::new();
    let cart = harness.cart_with(Some(customer), &[("acme", "TEE", 5_000, 2)]).await;

    // Live totals before paying.
    let quote = harness.checkout.quote(&cart).await.unwrap();
    assert_eq!(quote.totals.subtotal, usd(10_000));
    assert_eq!(quote.totals.tax, usd(1_000));
    assert_eq!(quote.amount_due(), usd(11_000));

    let request =
        CheckoutRequest::new(cart.id.clone(), "sc1").with_tender(Harness::card());
    let result = harness.checkout.checkout(&request).await.unwrap();

    assert_eq!(result.order.status, OrderStatus::Paid);
    assert_eq!(result.order.amount_captured, usd(11_000));
    assert_eq!(result.payments.len(), 1);
    assert_eq!(result.payments[0].status, PaymentStatus::Captured);

    // One shop, no split: it is owed the whole charge minus commission.
    let settlement = &result.order.settlement;
    assert_eq!(settlement.shops.len(), 1);
    assert_eq!(settlement.shops[0].gross, usd(11_000));
    assert_eq!(settlement.shops[0].platform_fee, usd(1_000)); // 10 % of goods, not of tax
    assert_eq!(settlement.shops[0].net, usd(10_000));
    assert!(settlement.funders.is_empty());
    assert_order_balances(&result.order);
}

/// ### Scenario 2 — single shop with a subsidised item
///
/// A third party funds $30 of a $100 order. The shopper pays $80, the funder is
/// billed $30, and the shop is still owed the full $110 — including tax on the
/// undiscounted price, because the shop's consideration never changed.
#[tokio::test]
async fn single_shop_with_payment_split() {
    let harness = Harness::new();
    harness
        .register_discount(
            Discount::amount_off("SUBSIDY30", "Employer benefit", usd(3_000))
                .funded_by(AccountId::from_string("acct_employer"), "benefit"),
        )
        .await;

    let cart = harness.cart_with(Some(CustomerId::new()), &[("acme", "TEE", 10_000, 1)]).await;
    let cart = harness.carts.apply_discount_code(&cart.id, "SUBSIDY30").await.unwrap();

    let quote = harness.checkout.quote(&cart).await.unwrap();
    assert_eq!(quote.totals.subsidy_discount, usd(3_000));
    assert_eq!(quote.totals.tax, usd(1_000), "tax is charged on the full price");
    assert_eq!(quote.amount_due(), usd(8_000));
    assert_eq!(quote.totals.merchant_gross, usd(11_000));

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "sc2").with_tender(Harness::card()))
        .await
        .unwrap();

    assert_eq!(result.order.status, OrderStatus::Paid);
    assert_eq!(result.order.amount_captured, usd(8_000));

    let settlement = &result.order.settlement;
    assert_eq!(settlement.collected_from_customer, usd(8_000));
    assert_eq!(settlement.collected_from_funders, usd(3_000));
    assert_eq!(settlement.shops[0].gross, usd(11_000));
    assert_eq!(settlement.shops[0].funded_by_customer, usd(8_000));
    assert_eq!(settlement.shops[0].funded_by_subsidy, usd(3_000));
    assert_eq!(settlement.funders.len(), 1);
    assert_eq!(settlement.funders[0].funder, AccountId::from_string("acct_employer"));
    assert_order_balances(&result.order);
}

/// ### Scenario 3 — several shops, subsidised, split settlement
///
/// A $15 platform promotion is prorated across three shops. Every shop's tax is
/// computed on its own undiscounted goods, and the three settlements plus the
/// funder charge account for every cent collected.
#[tokio::test]
async fn multiple_shops_with_payment_split() {
    let harness = Harness::new();
    harness
        .register_discount(
            Discount::amount_off("WELCOME15", "Platform promotion", usd(1_500))
                .funded_by(AccountId::from_string("acct_platform"), "welcome"),
        )
        .await;

    let cart = harness
        .cart_with(
            Some(CustomerId::new()),
            &[("bakery", "CAKE", 5_000, 1), ("florist", "ROSE", 3_000, 1), ("grocer", "FRUIT", 2_000, 1)],
        )
        .await;
    let cart = harness.carts.apply_discount_code(&cart.id, "WELCOME15").await.unwrap();

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "sc3").with_tender(Harness::card()))
        .await
        .unwrap();

    let settlement = &result.order.settlement;
    assert_eq!(settlement.shops.len(), 3);
    assert_eq!(settlement.collected_from_customer, usd(9_500)); // 100 - 15 + 10 tax
    assert_eq!(settlement.collected_from_funders, usd(1_500));

    // Each shop keeps its full price plus its own tax.
    assert_eq!(settlement.shop(&ShopId::from_string("bakery")).unwrap().gross, usd(5_500));
    assert_eq!(settlement.shop(&ShopId::from_string("florist")).unwrap().gross, usd(3_300));
    assert_eq!(settlement.shop(&ShopId::from_string("grocer")).unwrap().gross, usd(2_200));

    // The subsidy is split in proportion to each shop's share of the basket.
    let funder = &settlement.funders[0];
    assert_eq!(funder.amount, usd(1_500));
    assert_eq!(funder.per_shop["bakery"], usd(750));
    assert_eq!(funder.per_shop["florist"], usd(450));
    assert_eq!(funder.per_shop["grocer"], usd(300));

    assert_order_balances(&result.order);
}

/// ### Scenario 4 — several shops, split, then a full refund
///
/// Every shop is debited what it received, the funder's subsidy is reclaimed,
/// and the shopper's card is returned exactly what it paid.
#[tokio::test]
async fn multiple_shops_with_payment_split_and_full_refund() {
    let harness = Harness::new();
    harness
        .register_discount(
            Discount::percentage_off("SAVE20", "Platform promotion", 2_000)
                .funded_by(AccountId::from_string("acct_platform"), "promo"),
        )
        .await;

    let cart = harness
        .cart_with(
            Some(CustomerId::new()),
            &[("bakery", "CAKE", 6_000, 1), ("florist", "ROSE", 4_000, 1)],
        )
        .await;
    let cart = harness.carts.apply_discount_code(&cart.id, "SAVE20").await.unwrap();

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "sc4").with_tender(Harness::card()))
        .await
        .unwrap();
    let order_id = result.order.id.clone();
    let charged = result.order.amount_captured;
    assert_eq!(charged, usd(9_000)); // 100 - 20 + 10 tax

    let order = harness
        .checkout
        .refund(&order_id, &RefundRequest::full("sc4-refund"))
        .await
        .unwrap();

    assert_eq!(order.status, OrderStatus::Refunded);
    assert_eq!(order.amount_refunded, charged);

    let plan = &order.refunds[0].plan;
    assert_eq!(plan.total, usd(9_000), "the shopper gets back exactly what they paid");
    assert_eq!(plan.subsidy_reclaimed, usd(2_000), "the funder gets its promotion back");
    assert_eq!(plan.tax_refunded, usd(1_000));
    assert_eq!(plan.shops.len(), 2);
    // Shops give back everything, including the commission they never received.
    assert_eq!(plan.shops[0].gross.try_add(plan.shops[1].gross).unwrap(), usd(11_000));
    assert_eq!(
        plan.shops[0]
            .platform_fee_returned
            .try_add(plan.shops[1].platform_fee_returned)
            .unwrap(),
        usd(1_000)
    );
    assert_order_balances(&order);

    // A second full refund has nothing left to return.
    assert!(
        harness
            .checkout
            .refund(&order_id, &RefundRequest::full("sc4-refund-2"))
            .await
            .is_err()
    );
}

/// ### Scenario 5 — several shops, split, part-paid with shop credit
///
/// Shop credit is scoped to one shop, so it may only pay that shop's share even
/// when the balance would cover more.
#[tokio::test]
async fn multiple_shops_with_split_including_shop_credit() {
    let harness = Harness::new();
    let customer = CustomerId::new();
    harness
        .register_discount(
            Discount::amount_off("SUB10", "Platform promotion", usd(1_000))
                .funded_by(AccountId::from_string("acct_platform"), "promo"),
        )
        .await;
    // Far more credit than the bakery's share of the order.
    harness.grant_credit(&customer, "bakery", 50_000).await;

    let cart = harness
        .cart_with(
            Some(customer.clone()),
            &[("bakery", "CAKE", 6_000, 1), ("florist", "ROSE", 4_000, 1)],
        )
        .await;
    let cart = harness.carts.apply_discount_code(&cart.id, "SUB10").await.unwrap();
    let cart = harness.carts.set_apply_shop_credit(&cart.id, true).await.unwrap();

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "sc5").with_tender(Harness::card()))
        .await
        .unwrap();

    assert_eq!(result.order.status, OrderStatus::Paid);
    // Shopper owes 100 - 10 subsidy + 10 tax = 100.00, of which the bakery's
    // share is 66.00 - 6.00 subsidy = 60.00.
    assert_eq!(result.order.total(), usd(10_000));

    let credit_tender = result
        .order
        .tenders
        .iter()
        .find(|tender| matches!(tender.kind, payments::payment::TenderKind::ShopCredit { .. }))
        .expect("credit was used");
    assert_eq!(credit_tender.amount, usd(6_000), "capped at the bakery's share");
    assert_eq!(credit_tender.shop_allocation["bakery"], usd(6_000));
    assert!(!credit_tender.shop_allocation.contains_key("florist"));

    // The card covers the rest, and only the used credit was deducted.
    assert_eq!(result.payments.len(), 1);
    assert_eq!(result.payments[0].amount, usd(4_000));
    assert_eq!(harness.credit_balance(&customer, "bakery").await, usd(44_000));
    assert_order_balances(&result.order);
}

/// ### Scenario 6 — several shops, split, shop credit, then a partial refund
///
/// Returning one bakery item must return money to the bakery's credit *and* to
/// the card, in the same proportion they funded that shop's share.
#[tokio::test]
async fn multiple_shops_with_split_shop_credit_and_partial_refund() {
    let harness = Harness::new();
    let customer = CustomerId::new();
    harness.grant_credit(&customer, "bakery", 3_000).await;

    let cart = harness
        .cart_with(
            Some(customer.clone()),
            &[("bakery", "CAKE", 5_000, 2), ("florist", "ROSE", 4_000, 1)],
        )
        .await;
    let cart = harness.carts.set_apply_shop_credit(&cart.id, true).await.unwrap();

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "sc6").with_tender(Harness::card()))
        .await
        .unwrap();
    let order_id = result.order.id.clone();

    // Bakery share is 100.00 + 10.00 tax = 110.00; credit covers 30.00 of it.
    assert_eq!(result.order.total(), usd(15_400));
    assert_eq!(harness.credit_balance(&customer, "bakery").await, usd(0));

    // Return one of the two cakes: 50.00 + 5.00 tax = 55.00.
    let cake_line = result
        .order
        .quote
        .lines
        .iter()
        .find(|line| line.sku == "CAKE")
        .unwrap()
        .line_id
        .clone();

    let order = harness
        .checkout
        .refund(
            &order_id,
            &RefundRequest::lines(
                "sc6-refund",
                vec![RefundLineRequest { line_id: cake_line.clone(), quantity: 1 }],
            ),
        )
        .await
        .unwrap();

    assert_eq!(order.status, OrderStatus::PartiallyRefunded);
    let plan = &order.refunds[0].plan;
    assert_eq!(plan.total, usd(5_500));
    assert_eq!(plan.tax_refunded, usd(500));

    // The refund is shared between the two instruments that funded the bakery:
    // credit paid 30 of 110, the card 80 of 110, so of 55 they get 15 and 40.
    let credit_back = plan
        .tenders
        .iter()
        .find(|tender| matches!(tender.kind, payments::payment::TenderKind::ShopCredit { .. }))
        .map(|tender| tender.amount)
        .unwrap();
    let card_back = plan
        .tenders
        .iter()
        .find(|tender| matches!(tender.kind, payments::payment::TenderKind::Gateway { .. }))
        .map(|tender| tender.amount)
        .unwrap();
    assert_eq!(credit_back, usd(1_500));
    assert_eq!(card_back, usd(4_000));
    assert_eq!(credit_back.try_add(card_back).unwrap(), plan.total);
    assert_eq!(harness.credit_balance(&customer, "bakery").await, usd(1_500));

    // Only the florist item and one cake remain refundable.
    assert_eq!(order.refundable_amount().unwrap(), usd(9_900));
    assert_order_balances(&order);

    // Returning the second cake finishes the bakery's side of the order.
    let order = harness
        .checkout
        .refund(
            &order_id,
            &RefundRequest::lines(
                "sc6-refund-2",
                vec![RefundLineRequest { line_id: cake_line, quantity: 1 }],
            ),
        )
        .await
        .unwrap();
    assert_eq!(harness.credit_balance(&customer, "bakery").await, usd(3_000));
    assert_eq!(order.amount_refunded, usd(11_000));
    assert_order_balances(&order);
}

/// Gift card + shop credit + card in a single transaction, refunded in full.
#[tokio::test]
async fn three_instruments_in_one_transaction_round_trip() {
    let harness = Harness::new();
    let customer = CustomerId::new();
    let card = harness.issue_gift_card("GIFT-1111-2222", 4_000).await;
    harness.grant_credit(&customer, "bookstore", 2_000).await;

    let mut cart = harness
        .cart_with(
            Some(customer.clone()),
            &[("bookstore", "BOOK", 6_000, 1), ("record-shop", "LP", 4_000, 1)],
        )
        .await;
    cart.add_gift_card("GIFT-1111-2222");
    cart.set_apply_shop_credit(true);
    harness.repositories.carts.save(&cart).await.unwrap();

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "mt1").with_tender(Harness::card()))
        .await
        .unwrap();
    let order_id = result.order.id.clone();

    assert_eq!(result.order.total(), usd(11_000));
    assert_eq!(result.order.tenders.len(), 3);
    assert_eq!(harness.gift_card_balance(&card.id).await, usd(0));
    assert_eq!(harness.credit_balance(&customer, "bookstore").await, usd(0));

    // Gift card 40, credit 20, card the remaining 50.
    assert_eq!(result.payments[0].amount, usd(5_000));

    let order = harness
        .checkout
        .refund(&order_id, &RefundRequest::full("mt1-refund"))
        .await
        .unwrap();

    assert_eq!(order.status, OrderStatus::Refunded);
    assert_eq!(harness.gift_card_balance(&card.id).await, usd(4_000), "gift card restored");
    assert_eq!(
        harness.credit_balance(&customer, "bookstore").await,
        usd(2_000),
        "shop credit restored"
    );
    assert_order_balances(&order);
}

/// Authorise at checkout, capture as each shipment is delivered.
#[tokio::test]
async fn authorize_now_capture_on_delivery() {
    let harness = Harness::with_capture_mode(CaptureMode::Manual);
    let cart = harness
        .cart_with(
            Some(CustomerId::new()),
            &[("bakery", "CAKE", 6_000, 1), ("florist", "ROSE", 4_000, 1)],
        )
        .await;

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "cap1").with_tender(Harness::card()))
        .await
        .unwrap();
    let order_id = result.order.id.clone();

    // Funds are held, not taken.
    assert_eq!(result.order.status, OrderStatus::Authorized);
    assert_eq!(result.order.amount_captured, usd(0));
    assert_eq!(result.payments[0].status, PaymentStatus::Authorized);

    // Capture is refused until the merchant explicitly confirms it.
    let first_group = result.order.fulfillment_groups[0].id.clone();
    assert!(
        harness
            .checkout
            .capture_fulfillment_group(&order_id, &first_group)
            .await
            .is_err(),
        "unconfirmed captures must be rejected"
    );

    harness.checkout.confirm(&order_id).await.unwrap();

    let order = harness
        .checkout
        .capture_fulfillment_group(&order_id, &first_group)
        .await
        .unwrap();
    assert_eq!(order.status, OrderStatus::PartiallyCaptured);
    assert_eq!(order.amount_captured, usd(6_600));

    let second_group = order.fulfillment_groups[1].id.clone();
    let order = harness
        .checkout
        .capture_fulfillment_group(&order_id, &second_group)
        .await
        .unwrap();
    assert_eq!(order.status, OrderStatus::Paid);
    assert_eq!(order.amount_captured, usd(11_000));
}

/// A declined card must not drain the shopper's gift card.
#[tokio::test]
async fn a_declined_card_unwinds_stored_value() {
    let harness = Harness::new();
    let card = harness.issue_gift_card("GIFT-3333-4444", 3_000).await;

    let mut cart = harness
        .cart_with(Some(CustomerId::new()), &[("acme", "TEE", 10_000, 1)])
        .await;
    cart.add_gift_card("GIFT-3333-4444");
    harness.repositories.carts.save(&cart).await.unwrap();

    let declined = TenderOffer::gateway(
        GatewayId::from_static("mock"),
        single_use("tok_decline"),
        "declined card",
    );
    let error = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "fail1").with_tender(declined))
        .await
        .unwrap_err();

    assert!(matches!(error, Error::Declined { .. }));
    assert_eq!(
        harness.gift_card_balance(&card.id).await,
        usd(3_000),
        "the gift card must be untouched after a failed checkout"
    );
}

/// Cancelling an authorised order releases the hold and the stored value.
#[tokio::test]
async fn cancelling_releases_holds_and_stored_value() {
    let harness = Harness::with_capture_mode(CaptureMode::Manual);
    let customer = CustomerId::new();
    harness.grant_credit(&customer, "acme", 2_000).await;

    let mut cart = harness.cart_with(Some(customer.clone()), &[("acme", "TEE", 10_000, 1)]).await;
    cart.set_apply_shop_credit(true);
    harness.repositories.carts.save(&cart).await.unwrap();

    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "cancel1").with_tender(Harness::card()))
        .await
        .unwrap();
    assert_eq!(harness.credit_balance(&customer, "acme").await, usd(0));

    let order = harness.checkout.cancel(&result.order.id).await.unwrap();
    assert_eq!(order.status, OrderStatus::Canceled);
    assert_eq!(harness.credit_balance(&customer, "acme").await, usd(2_000));
}

/// Fraud screening runs before the gateway is contacted.
#[tokio::test]
async fn blocked_orders_never_reach_the_gateway() {
    use payments::fraud::FraudPolicy;
    use std::sync::Arc;

    let harness = Harness::new();
    let checkout = harness
        .checkout
        .clone()
        .with_fraud_engine(Arc::new(RuleBasedFraudEngine::with_policy(FraudPolicy {
            max_instruments_per_hour: 1,
            block_score: 40,
            ..Default::default()
        })));

    let cart = harness.cart_with(Some(CustomerId::new()), &[("acme", "TEE", 1_000, 1)]).await;
    let mut request =
        CheckoutRequest::new(cart.id.clone(), "risk1").with_tender(Harness::card());
    request.risk.distinct_instruments_last_hour = 9;

    let error = checkout.checkout(&request).await.unwrap_err();
    assert!(matches!(error, Error::Declined { code: DeclineCode::Fraudulent, .. }));
    // No transaction was created at the gateway.
    assert!(harness.gateway.transaction_captured("txn_00000001").is_none());
}

/// Live re-pricing: totals track cart edits without any persistence round trip.
#[tokio::test]
async fn totals_update_live_as_the_cart_changes() {
    let harness = Harness::new();
    harness
        .register_discount(Discount::percentage_off("TENOFF", "10% off", 1_000))
        .await;

    let cart = harness.cart_with(Some(CustomerId::new()), &[("acme", "TEE", 5_000, 1)]).await;
    assert_eq!(harness.checkout.quote(&cart).await.unwrap().amount_due(), usd(5_500));

    let line = cart.items[0].id.clone();
    let cart = harness.carts.set_quantity(&cart.id, &line, 3).await.unwrap();
    assert_eq!(harness.checkout.quote(&cart).await.unwrap().amount_due(), usd(16_500));

    let cart = harness.carts.apply_discount_code(&cart.id, "TENOFF").await.unwrap();
    let quote = harness.checkout.quote(&cart).await.unwrap();
    assert_eq!(quote.totals.merchant_discount, usd(1_500));
    assert_eq!(quote.amount_due(), usd(14_850));

    // An unknown code is reported, not fatal.
    let cart = harness.carts.apply_discount_code(&cart.id, "NOPE").await.unwrap();
    assert_eq!(harness.checkout.quote(&cart).await.unwrap().amount_due(), usd(14_850));
}
