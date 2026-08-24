//! End-to-end webhook handling: verify, deduplicate, reconcile.

mod common;

use std::sync::Arc;

use common::{Harness, usd};

use payments::checkout::OrderWebhookHandler;
use payments::gateway::CaptureMode;
use payments::prelude::*;
use payments::webhook::WebhookOutcome;
use serde_json::json;

/// A manual-capture order is captured out of band; the webhook brings the order
/// up to date without the application polling.
#[tokio::test]
async fn a_capture_webhook_advances_the_order() {
    let harness = Harness::with_capture_mode(CaptureMode::Manual);
    let gateways = GatewayRegistry::new().register(harness.gateway.clone());

    let processor = WebhookProcessor::new(Arc::new(InMemoryProcessedEventStore::new()))
        .register_gateway(harness.gateway.clone())
        .register_handler(Arc::new(OrderWebhookHandler::new(
            gateways,
            harness.repositories.clone(),
        )));

    let cart = harness.cart_with(Some(CustomerId::new()), &[("acme", "TEE", 10_000, 1)]).await;
    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "wh1").with_tender(Harness::card()))
        .await
        .unwrap();
    let order_id = result.order.id.clone();
    assert_eq!(result.order.status, OrderStatus::Authorized);

    // The merchant captures through the gateway's own dashboard, not through us.
    let transaction_id = result.payments[0].transaction_id.clone().unwrap();
    harness
        .gateway
        .capture(&payments::gateway::CaptureRequest {
            idempotency_key: "external".into(),
            transaction_id: transaction_id.clone(),
            amount: usd(11_000),
            final_capture: true,
        })
        .await
        .unwrap();

    // Our order still thinks nothing was captured.
    assert_eq!(harness.checkout.load_order(&order_id).await.unwrap().amount_captured, usd(0));

    // The gateway tells us about it.
    let payload = json!({
        "id": "evt_capture_1",
        "type": "payment.captured",
        "data": { "transaction_id": transaction_id }
    })
    .to_string()
    .into_bytes();
    let headers = harness.gateway.sign_webhook(&payload);

    let outcome = processor
        .process(&GatewayId::from_static("mock"), &payload, &headers)
        .await
        .unwrap();
    assert_eq!(outcome, WebhookOutcome::Processed);

    let order = harness.checkout.load_order(&order_id).await.unwrap();
    assert_eq!(order.amount_captured, usd(11_000));
    assert_eq!(order.status, OrderStatus::Paid);

    // A redelivery of the same event must not double-count the capture.
    let outcome = processor
        .process(&GatewayId::from_static("mock"), &payload, &headers)
        .await
        .unwrap();
    assert_eq!(outcome, WebhookOutcome::Duplicate);
    assert_eq!(
        harness.checkout.load_order(&order_id).await.unwrap().amount_captured,
        usd(11_000)
    );
}

/// An unsigned or tampered payload never reaches the handler.
#[tokio::test]
async fn unverified_webhooks_are_rejected() {
    let harness = Harness::new();
    let gateways = GatewayRegistry::new().register(harness.gateway.clone());
    let processor = WebhookProcessor::new(Arc::new(InMemoryProcessedEventStore::new()))
        .register_gateway(harness.gateway.clone())
        .register_handler(Arc::new(OrderWebhookHandler::new(
            gateways,
            harness.repositories.clone(),
        )));

    let payload = json!({"id": "evt_1", "type": "payment.captured"}).to_string().into_bytes();
    let id = GatewayId::from_static("mock");

    // No signature at all.
    assert!(processor.process(&id, &payload, &Headers::new()).await.is_err());

    // A signature for a different body.
    let headers = harness.gateway.sign_webhook(b"something else");
    assert!(processor.process(&id, &payload, &headers).await.is_err());
}

/// Events about transactions we do not know are acknowledged, not retried.
#[tokio::test]
async fn unknown_transactions_are_acknowledged() {
    let harness = Harness::new();
    let gateways = GatewayRegistry::new().register(harness.gateway.clone());
    let processor = WebhookProcessor::new(Arc::new(InMemoryProcessedEventStore::new()))
        .register_gateway(harness.gateway.clone())
        .register_handler(Arc::new(OrderWebhookHandler::new(
            gateways,
            harness.repositories.clone(),
        )));

    let payload = json!({
        "id": "evt_orphan",
        "type": "payment.captured",
        "data": {"transaction_id": "txn_not_ours"}
    })
    .to_string()
    .into_bytes();
    let headers = harness.gateway.sign_webhook(&payload);

    let outcome = processor
        .process(&GatewayId::from_static("mock"), &payload, &headers)
        .await
        .unwrap();
    assert_eq!(outcome, WebhookOutcome::Processed);
}

/// A failure notification marks the payment and the order as failed.
#[tokio::test]
async fn failure_webhooks_fail_the_order() {
    let harness = Harness::with_capture_mode(CaptureMode::Manual);
    let gateways = GatewayRegistry::new().register(harness.gateway.clone());
    let processor = WebhookProcessor::new(Arc::new(InMemoryProcessedEventStore::new()))
        .register_gateway(harness.gateway.clone())
        .register_handler(Arc::new(OrderWebhookHandler::new(
            gateways,
            harness.repositories.clone(),
        )));

    let cart = harness.cart_with(Some(CustomerId::new()), &[("acme", "TEE", 5_000, 1)]).await;
    let result = harness
        .checkout
        .checkout(&CheckoutRequest::new(cart.id.clone(), "wh2").with_tender(Harness::card()))
        .await
        .unwrap();
    let transaction_id = result.payments[0].transaction_id.clone().unwrap();

    let payload = json!({
        "id": "evt_fail_1",
        "type": "payment.failed",
        "data": {"transaction_id": transaction_id}
    })
    .to_string()
    .into_bytes();
    let headers = harness.gateway.sign_webhook(&payload);
    processor
        .process(&GatewayId::from_static("mock"), &payload, &headers)
        .await
        .unwrap();

    let payment = harness
        .repositories
        .payments
        .find_by_transaction(&transaction_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(payment.status, PaymentStatus::Failed);
    assert!(payment.failure_message.is_some());
}
