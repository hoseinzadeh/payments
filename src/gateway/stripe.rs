//! Stripe adapter (Payment Intents API).
//!
//! Enabled with the `stripe` feature. The adapter is transport-agnostic: supply
//! an [`HttpTransport`] built on whatever HTTP client your service already
//! uses. It speaks Stripe's form-encoded API, forwards idempotency keys on the
//! `Idempotency-Key` header, and normalises Stripe's error and decline codes
//! into [`DeclineCode`] so the rest of the crate stays provider-neutral.
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use payments::gateway::stripe::StripeGateway;
//! # use payments::gateway::http::HttpTransport;
//! # use payments::secret::SecretString;
//! # fn wire(transport: Arc<dyn HttpTransport>) {
//! let stripe = StripeGateway::new(
//!     transport,
//!     SecretString::new(std::env::var("STRIPE_SECRET_KEY").unwrap()),
//! )
//! .with_webhook_secret(SecretString::new("whsec_..."));
//! # }
//! ```

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::Value;
use std::sync::Arc;

use crate::error::{DeclineCode, Error, Result};
use crate::gateway::http::{FormBody, HttpRequest, HttpResponse, HttpTransport};
use crate::gateway::{
    AuthorizeRequest, AuthorizeResponse, CancelRequest, Capabilities, CaptureMode, CaptureRequest,
    CaptureResponse, CustomerRequest, GatewayEvent, GatewayEventKind, GatewayId,
    GatewayRefundRequest, GatewayRefundResponse, InstrumentRef, NextAction, PaymentGateway,
    RefundReason, RefundStatus, TransactionStatus, VaultRequest,
};
use crate::money::{Currency, Money};
use crate::payment::method::{
    CardSummary, ChargeInitiator, PaymentMethodKind, PaymentMethodRef,
};
use crate::secret::SecretString;
use crate::webhook::{Headers, hmac_sha256_hex, verify_timestamp_freshness};

/// Identifier of the Stripe adapter.
pub const STRIPE_GATEWAY_ID: GatewayId = GatewayId::from_static("stripe");

/// Stripe's Payment Intents adapter.
pub struct StripeGateway {
    transport: Arc<dyn HttpTransport>,
    secret_key: SecretString,
    webhook_secret: Option<SecretString>,
    base_url: String,
    api_version: String,
    signature_tolerance_seconds: i64,
}

impl std::fmt::Debug for StripeGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StripeGateway")
            .field("base_url", &self.base_url)
            .field("api_version", &self.api_version)
            .finish_non_exhaustive()
    }
}

impl StripeGateway {
    /// Build an adapter from a transport and a secret key.
    pub fn new(transport: Arc<dyn HttpTransport>, secret_key: SecretString) -> Self {
        Self {
            transport,
            secret_key,
            webhook_secret: None,
            base_url: "https://api.stripe.com".to_owned(),
            api_version: "2024-06-20".to_owned(),
            signature_tolerance_seconds: 300,
        }
    }

    /// Builder: set the webhook signing secret (`whsec_…`).
    pub fn with_webhook_secret(mut self, secret: SecretString) -> Self {
        self.webhook_secret = Some(secret);
        self
    }

    /// Builder: point at a different base URL (a local mock, a proxy).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Builder: pin the Stripe API version.
    pub fn with_api_version(mut self, version: impl Into<String>) -> Self {
        self.api_version = version.into();
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn request(&self, request: HttpRequest, idempotency_key: Option<&str>) -> HttpRequest {
        let mut request = request
            .with_bearer_auth(&self.secret_key)
            .with_header("stripe-version", self.api_version.clone());
        if let Some(key) = idempotency_key {
            request = request.with_header("idempotency-key", key);
        }
        request
    }

    async fn send(&self, request: HttpRequest) -> Result<Value> {
        let response = self.transport.execute(request).await?;
        self.interpret(response)
    }

    fn interpret(&self, response: HttpResponse) -> Result<Value> {
        let body = response.json_body().unwrap_or(Value::Null);
        if response.is_success() {
            return Ok(body);
        }
        Err(self.map_error(&body, response.is_retryable()))
    }

    /// Translate a Stripe error payload into a crate error.
    fn map_error(&self, body: &Value, retryable: bool) -> Error {
        let error = &body["error"];
        let message = error["message"]
            .as_str()
            .unwrap_or("Stripe returned an error with no message")
            .to_owned();
        let code = error["code"].as_str();
        let decline_code = error["decline_code"].as_str();

        if error["type"].as_str() == Some("card_error") || decline_code.is_some() {
            return Error::Declined {
                code: map_decline_code(decline_code.or(code)),
                message,
            };
        }
        Error::Gateway {
            gateway: STRIPE_GATEWAY_ID.to_string(),
            provider_code: code.map(str::to_owned),
            message,
            retryable: retryable || error["type"].as_str() == Some("api_error"),
        }
    }

    fn parse_intent(&self, intent: &Value) -> Result<AuthorizeResponse> {
        let currency = Currency::from_code(
            intent["currency"].as_str().unwrap_or("usd"),
        )?;
        let status = intent["status"].as_str().unwrap_or_default();

        let amount = intent["amount"].as_i64().unwrap_or(0);
        let capturable = intent["amount_capturable"].as_i64().unwrap_or(0);
        let received = intent["amount_received"].as_i64().unwrap_or(0);

        let transaction_status = match status {
            "succeeded" => TransactionStatus::Captured,
            "requires_capture" => TransactionStatus::Authorized,
            "requires_action" | "requires_confirmation" => TransactionStatus::RequiresAction,
            "processing" => TransactionStatus::Pending,
            "canceled" => TransactionStatus::Canceled,
            _ => TransactionStatus::Failed,
        };

        let authorized = match transaction_status {
            TransactionStatus::Authorized => capturable,
            TransactionStatus::Captured => amount,
            TransactionStatus::PartiallyCaptured => capturable + received,
            _ => 0,
        };

        let next_action = match &intent["next_action"] {
            Value::Object(action) => action
                .get("redirect_to_url")
                .and_then(|redirect| redirect["url"].as_str())
                .map(|url| NextAction::Redirect { url: url.to_owned() })
                .or_else(|| {
                    intent["client_secret"]
                        .as_str()
                        .map(|secret| NextAction::UseSdk { client_secret: secret.to_owned() })
                }),
            _ => None,
        };

        Ok(AuthorizeResponse {
            transaction_id: intent["id"].as_str().unwrap_or_default().to_owned(),
            status: transaction_status,
            amount_authorized: Money::from_minor(authorized, currency),
            amount_captured: Money::from_minor(received, currency),
            // Stripe holds card authorisations for 7 days.
            expires_at: intent["created"]
                .as_i64()
                .and_then(|created| Utc.timestamp_opt(created, 0).single())
                .map(|created| created + chrono::Duration::days(7)),
            next_action,
            processor_reference: intent["latest_charge"].as_str().map(str::to_owned),
            raw: Some(intent.clone()),
        })
    }
}

fn map_decline_code(code: Option<&str>) -> DeclineCode {
    match code.unwrap_or_default() {
        "insufficient_funds" => DeclineCode::InsufficientFunds,
        "expired_card" => DeclineCode::ExpiredCard,
        "incorrect_number" | "invalid_number" => DeclineCode::IncorrectNumber,
        "incorrect_cvc" | "invalid_cvc" => DeclineCode::IncorrectCvc,
        "incorrect_zip" => DeclineCode::IncorrectPostalCode,
        "fraudulent" | "merchant_blacklist" | "pickup_card" => DeclineCode::Fraudulent,
        "lost_card" | "stolen_card" => DeclineCode::LostOrStolenCard,
        "card_velocity_exceeded" | "withdrawal_count_limit_exceeded" => DeclineCode::LimitExceeded,
        "authentication_required" => DeclineCode::AuthenticationRequired,
        "processing_error" | "issuer_not_available" | "try_again_later" => {
            DeclineCode::ProcessingError
        }
        "currency_not_supported" | "card_not_supported" => DeclineCode::Unsupported,
        _ => DeclineCode::GenericDecline,
    }
}

fn refund_reason(reason: RefundReason) -> Option<&'static str> {
    match reason {
        RefundReason::Duplicate => Some("duplicate"),
        RefundReason::Fraudulent => Some("fraudulent"),
        RefundReason::RequestedByCustomer
        | RefundReason::OrderCanceled
        | RefundReason::Unavailable => Some("requested_by_customer"),
        RefundReason::Other => None,
    }
}

#[async_trait]
impl PaymentGateway for StripeGateway {
    fn id(&self) -> GatewayId {
        STRIPE_GATEWAY_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            delayed_capture: true,
            partial_capture: true,
            // Stripe closes the authorisation after the first capture.
            multi_capture: false,
            refunds: true,
            partial_refunds: true,
            connected_accounts: true,
            application_fees: true,
            stored_credentials: true,
            webhooks: true,
            disputes: true,
            three_d_secure: true,
            authorization_validity: chrono::Duration::days(7),
            currencies: Default::default(),
        }
    }

    async fn upsert_customer(&self, request: &CustomerRequest) -> Result<String> {
        let mut form = FormBody::new();
        form.set("metadata[customer_id]", request.customer_id.to_string());
        form.set_opt("email", request.email.clone());
        form.set_opt("name", request.name.clone());
        for (key, value) in &request.metadata {
            form.set(format!("metadata[{key}]"), value.clone());
        }

        let http = self.request(
            HttpRequest::post_form(self.url("/v1/customers"), &form),
            Some(&format!("customer:{}", request.customer_id)),
        );
        let body = self.send(http).await?;
        body["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| Error::internal("Stripe customer response had no id"))
    }

    async fn vault_payment_method(&self, request: &VaultRequest) -> Result<PaymentMethodRef> {
        let mut form = FormBody::new();
        form.set("customer", request.gateway_customer_id.clone());

        let http = self.request(
            HttpRequest::post_form(
                self.url(&format!("/v1/payment_methods/{}/attach", request.setup_token)),
                &form,
            ),
            Some(&format!("vault:{}", request.setup_token)),
        );
        let body = self.send(http).await?;

        let card = &body["card"];
        let kind = if card.is_object() {
            let mut summary = CardSummary::new(
                card["brand"].as_str().unwrap_or("unknown"),
                card["last4"].as_str().unwrap_or("0000"),
                card["exp_month"].as_u64().unwrap_or(12) as u32,
                card["exp_year"].as_i64().unwrap_or(2099) as i32,
            )?;
            summary.country = card["country"].as_str().map(str::to_owned);
            summary.funding = card["funding"].as_str().map(str::to_owned);
            summary.fingerprint = card["fingerprint"].as_str().map(str::to_owned);
            PaymentMethodKind::Card(summary)
        } else {
            PaymentMethodKind::Other {
                provider_type: body["type"].as_str().unwrap_or("unknown").to_owned(),
            }
        };

        let mut method = PaymentMethodRef::new(
            request.customer_id.clone(),
            self.id(),
            body["id"].as_str().unwrap_or_default(),
            kind,
        )
        .with_gateway_customer(request.gateway_customer_id.clone());
        method.reusable = request.allow_off_session;
        method.metadata = request.metadata.clone();
        Ok(method)
    }

    async fn detach_payment_method(&self, token: &str) -> Result<()> {
        let http = self.request(
            HttpRequest::post_form(
                self.url(&format!("/v1/payment_methods/{token}/detach")),
                &FormBody::new(),
            ),
            None,
        );
        self.send(http).await.map(|_| ())
    }

    async fn authorize(&self, request: &AuthorizeRequest) -> Result<AuthorizeResponse> {
        request.validate()?;

        let mut form = FormBody::new();
        form.set("amount", request.amount.minor().to_string());
        form.set("currency", request.amount.currency().code().to_lowercase());
        form.set("confirm", "true");
        form.set(
            "capture_method",
            match request.capture_mode {
                CaptureMode::Automatic => "automatic",
                CaptureMode::Manual => "manual",
            },
        );

        match &request.instrument {
            InstrumentRef::SingleUseToken { token } => {
                form.set("payment_method", token.clone());
            }
            InstrumentRef::Vaulted { token, customer_token } => {
                form.set("payment_method", token.clone());
                form.set("customer", customer_token.clone());
            }
        }

        if request.initiator == ChargeInitiator::MerchantOffSession {
            form.set("off_session", "true");
        }
        form.set_opt("statement_descriptor", request.statement_descriptor.clone());
        form.set_opt("description", request.description.clone());
        form.set_opt("return_url", request.return_url.clone());

        if let Some(fee) = request.application_fee
            && fee.is_positive()
        {
            form.set("application_fee_amount", fee.minor().to_string());
        }

        // Stripe routes a charge to at most one connected account inline.
        // Multi-recipient splits therefore use a transfer group, and the
        // per-shop transfers are created separately after the charge settles.
        match request.transfers.len() {
            0 => {}
            1 => {
                let transfer = &request.transfers[0];
                form.set("transfer_data[destination]", transfer.destination.to_string());
                form.set("transfer_data[amount]", transfer.amount.minor().to_string());
                if let Some(group) = &transfer.transfer_group {
                    form.set("transfer_group", group.clone());
                }
            }
            _ => {
                let group = request
                    .transfers
                    .iter()
                    .find_map(|transfer| transfer.transfer_group.clone())
                    .or_else(|| request.order_id.as_ref().map(|id| id.to_string()))
                    .unwrap_or_else(|| request.idempotency_key.clone());
                form.set("transfer_group", group);
            }
        }

        if let Some(order_id) = &request.order_id {
            form.set("metadata[order_id]", order_id.to_string());
        }
        if let Some(customer_id) = &request.customer_id {
            form.set("metadata[customer_id]", customer_id.to_string());
        }
        for (key, value) in &request.metadata {
            form.set(format!("metadata[{key}]"), value.clone());
        }

        let http = self.request(
            HttpRequest::post_form(self.url("/v1/payment_intents"), &form),
            Some(&request.idempotency_key),
        );
        let body = self.send(http).await?;
        self.parse_intent(&body)
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResponse> {
        let mut form = FormBody::new();
        form.set("amount_to_capture", request.amount.minor().to_string());

        let http = self.request(
            HttpRequest::post_form(
                self.url(&format!("/v1/payment_intents/{}/capture", request.transaction_id)),
                &form,
            ),
            Some(&request.idempotency_key),
        );
        let body = self.send(http).await?;
        let intent = self.parse_intent(&body)?;

        Ok(CaptureResponse {
            capture_reference: intent
                .processor_reference
                .clone()
                .unwrap_or_else(|| intent.transaction_id.clone()),
            status: intent.status,
            amount_captured: request.amount,
            total_captured: intent.amount_captured,
            raw: intent.raw,
        })
    }

    async fn cancel(&self, request: &CancelRequest) -> Result<AuthorizeResponse> {
        let mut form = FormBody::new();
        form.set_opt("cancellation_reason", request.reason.clone());

        let http = self.request(
            HttpRequest::post_form(
                self.url(&format!("/v1/payment_intents/{}/cancel", request.transaction_id)),
                &form,
            ),
            Some(&request.idempotency_key),
        );
        let body = self.send(http).await?;
        self.parse_intent(&body)
    }

    async fn refund(&self, request: &GatewayRefundRequest) -> Result<GatewayRefundResponse> {
        let mut form = FormBody::new();
        form.set("payment_intent", request.transaction_id.clone());
        form.set("amount", request.amount.minor().to_string());
        form.set_opt("reason", refund_reason(request.reason));
        if request.reverse_transfers {
            form.set("reverse_transfer", "true");
        }
        if request.refund_application_fee {
            form.set("refund_application_fee", "true");
        }
        for (key, value) in &request.metadata {
            form.set(format!("metadata[{key}]"), value.clone());
        }

        let http = self.request(
            HttpRequest::post_form(self.url("/v1/refunds"), &form),
            Some(&request.idempotency_key),
        );
        let body = self.send(http).await?;

        let currency = Currency::from_code(body["currency"].as_str().unwrap_or("usd"))?;
        Ok(GatewayRefundResponse {
            refund_reference: body["id"].as_str().unwrap_or_default().to_owned(),
            amount: Money::from_minor(body["amount"].as_i64().unwrap_or(0), currency),
            status: match body["status"].as_str().unwrap_or_default() {
                "succeeded" => RefundStatus::Succeeded,
                "pending" | "requires_action" => RefundStatus::Pending,
                "canceled" => RefundStatus::Canceled,
                _ => RefundStatus::Failed,
            },
            raw: Some(body),
        })
    }

    async fn fetch_transaction(&self, transaction_id: &str) -> Result<AuthorizeResponse> {
        let http = self.request(
            HttpRequest::get(self.url(&format!("/v1/payment_intents/{transaction_id}"))),
            None,
        );
        let body = self.send(http).await?;
        self.parse_intent(&body)
    }

    async fn parse_webhook(&self, payload: &[u8], headers: &Headers) -> Result<GatewayEvent> {
        let secret = self.webhook_secret.as_ref().ok_or_else(|| {
            Error::configuration("no Stripe webhook signing secret configured")
        })?;

        // Stripe-Signature: t=<timestamp>,v1=<hex>,v1=<hex>
        let header = headers.require("stripe-signature")?;
        let mut timestamp: Option<i64> = None;
        let mut signatures: Vec<&str> = Vec::new();
        for part in header.split(',') {
            match part.trim().split_once('=') {
                Some(("t", value)) => timestamp = value.parse().ok(),
                Some(("v1", value)) => signatures.push(value),
                _ => {}
            }
        }
        let timestamp = timestamp.ok_or_else(|| {
            Error::WebhookVerification("Stripe-Signature has no timestamp".to_owned())
        })?;
        verify_timestamp_freshness(timestamp, Utc::now(), self.signature_tolerance_seconds)?;

        // The signed payload is "<timestamp>.<raw body>".
        let mut signed = Vec::with_capacity(payload.len() + 16);
        signed.extend_from_slice(timestamp.to_string().as_bytes());
        signed.push(b'.');
        signed.extend_from_slice(payload);
        let expected = hmac_sha256_hex(secret, &signed);

        let matched = signatures.iter().any(|candidate| {
            use subtle::ConstantTimeEq;
            let result: bool = expected.as_bytes().ct_eq(candidate.trim().as_bytes()).into();
            result
        });
        if !matched {
            return Err(Error::WebhookVerification(
                "no Stripe-Signature v1 entry matched".to_owned(),
            ));
        }

        let body: Value = serde_json::from_slice(payload)
            .map_err(|error| Error::WebhookVerification(format!("invalid JSON body: {error}")))?;
        let provider_type = body["type"].as_str().unwrap_or("unknown").to_owned();
        let object = &body["data"]["object"];

        let kind = match provider_type.as_str() {
            "payment_intent.amount_capturable_updated" => GatewayEventKind::PaymentAuthorized,
            "payment_intent.succeeded" | "charge.captured" => GatewayEventKind::PaymentCaptured,
            "payment_intent.payment_failed" | "charge.failed" => GatewayEventKind::PaymentFailed,
            "payment_intent.canceled" => GatewayEventKind::PaymentCanceled,
            "charge.expired" => GatewayEventKind::AuthorizationExpired,
            "charge.refunded" | "refund.created" => GatewayEventKind::RefundSucceeded,
            "refund.failed" => GatewayEventKind::RefundFailed,
            "charge.dispute.created" => GatewayEventKind::DisputeOpened,
            "charge.dispute.closed" => GatewayEventKind::DisputeClosed {
                won: object["status"].as_str() == Some("won"),
            },
            "payout.paid" => GatewayEventKind::PayoutPaid,
            other => GatewayEventKind::Unknown { provider_type: other.to_owned() },
        };

        let amount = match (object["amount"].as_i64(), object["currency"].as_str()) {
            (Some(minor), Some(code)) => {
                Currency::from_code(code).ok().map(|currency| Money::from_minor(minor, currency))
            }
            _ => None,
        };

        let occurred_at: DateTime<Utc> = body["created"]
            .as_i64()
            .and_then(|created| Utc.timestamp_opt(created, 0).single())
            .unwrap_or_else(Utc::now);

        Ok(GatewayEvent {
            provider_event_id: body["id"].as_str().unwrap_or_default().to_owned(),
            gateway: self.id(),
            kind,
            transaction_id: object["payment_intent"]
                .as_str()
                .or_else(|| object["id"].as_str())
                .map(str::to_owned),
            amount,
            occurred_at,
            raw: Some(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::http::MockTransport;
    use crate::gateway::TransferInstruction;
    use crate::ids::{AccountId, OrderId};
    use serde_json::json;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn gateway(transport: Arc<MockTransport>) -> StripeGateway {
        StripeGateway::new(transport, SecretString::new("sk_test_123"))
            .with_webhook_secret(SecretString::new("whsec_test"))
    }

    fn intent(status: &str, amount: i64, capturable: i64, received: i64) -> Value {
        json!({
            "id": "pi_123",
            "object": "payment_intent",
            "status": status,
            "currency": "usd",
            "amount": amount,
            "amount_capturable": capturable,
            "amount_received": received,
            "created": Utc::now().timestamp(),
            "latest_charge": "ch_123"
        })
    }

    #[tokio::test]
    async fn authorize_builds_the_expected_form_and_headers() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(200, intent("requires_capture", 10_000, 10_000, 0)));
        let stripe = gateway(transport.clone());

        let mut request = AuthorizeRequest::new(
            "idem-1",
            usd(10_000),
            InstrumentRef::Vaulted { token: "pm_1".into(), customer_token: "cus_1".into() },
        )
        .manual_capture()
        .for_order(OrderId::from_string("ord_9"));
        request.initiator = ChargeInitiator::MerchantOffSession;
        request.statement_descriptor = Some("SHOP".into());

        let response = stripe.authorize(&request).await.unwrap();
        assert_eq!(response.status, TransactionStatus::Authorized);
        assert_eq!(response.amount_authorized, usd(10_000));
        assert!(response.expires_at.is_some());

        let sent = transport.last_request().unwrap();
        assert_eq!(sent.url, "https://api.stripe.com/v1/payment_intents");
        assert_eq!(sent.headers.get("idempotency-key").unwrap(), "idem-1");
        assert_eq!(sent.headers.get("authorization").unwrap(), "Bearer sk_test_123");

        let body = sent.body_string();
        assert!(body.contains("amount=10000"));
        assert!(body.contains("currency=usd"));
        assert!(body.contains("capture_method=manual"));
        assert!(body.contains("off_session=true"));
        assert!(body.contains("payment_method=pm_1"));
        assert!(body.contains("customer=cus_1"));
        assert!(body.contains("metadata%5Border_id%5D=ord_9"));
    }

    #[tokio::test]
    async fn a_single_split_uses_transfer_data_and_many_use_a_transfer_group() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(200, intent("succeeded", 10_000, 0, 10_000)));
        let stripe = gateway(transport.clone());

        let one = AuthorizeRequest::new(
            "idem-1",
            usd(10_000),
            InstrumentRef::SingleUseToken { token: "pm_1".into() },
        )
        .with_transfers(vec![TransferInstruction {
            destination: AccountId::from_string("acct_shop1"),
            amount: usd(9_000),
            description: None,
            transfer_group: Some("grp_1".into()),
        }])
        .with_application_fee(usd(1_000));

        stripe.authorize(&one).await.unwrap();
        let body = transport.last_request().unwrap().body_string();
        assert!(body.contains("transfer_data%5Bdestination%5D=acct_shop1"));
        assert!(body.contains("transfer_data%5Bamount%5D=9000"));
        assert!(body.contains("application_fee_amount=1000"));

        transport.push_response(HttpResponse::json(200, intent("succeeded", 10_000, 0, 10_000)));
        let many = AuthorizeRequest::new(
            "idem-2",
            usd(10_000),
            InstrumentRef::SingleUseToken { token: "pm_1".into() },
        )
        .for_order(OrderId::from_string("ord_7"))
        .with_transfers(vec![
            TransferInstruction {
                destination: AccountId::from_string("acct_a"),
                amount: usd(5_000),
                description: None,
                transfer_group: None,
            },
            TransferInstruction {
                destination: AccountId::from_string("acct_b"),
                amount: usd(4_000),
                description: None,
                transfer_group: None,
            },
        ]);
        stripe.authorize(&many).await.unwrap();
        let body = transport.last_request().unwrap().body_string();
        assert!(body.contains("transfer_group=ord_7"));
        assert!(!body.contains("transfer_data"));
    }

    #[tokio::test]
    async fn card_errors_become_typed_declines() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            402,
            json!({"error": {
                "type": "card_error",
                "code": "card_declined",
                "decline_code": "insufficient_funds",
                "message": "Your card has insufficient funds."
            }}),
        ));
        let stripe = gateway(transport);

        let error = stripe
            .authorize(&AuthorizeRequest::new(
                "idem-1",
                usd(100),
                InstrumentRef::SingleUseToken { token: "pm_1".into() },
            ))
            .await
            .unwrap_err();

        match error {
            Error::Declined { code, .. } => assert_eq!(code, DeclineCode::InsufficientFunds),
            other => panic!("expected a decline, got {other}"),
        }
    }

    #[tokio::test]
    async fn api_errors_are_marked_retryable() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            500,
            json!({"error": {"type": "api_error", "message": "internal"}}),
        ));
        let stripe = gateway(transport);
        let error = stripe.fetch_transaction("pi_123").await.unwrap_err();
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn requires_action_surfaces_the_redirect() {
        let transport = Arc::new(MockTransport::new());
        let mut body = intent("requires_action", 10_000, 0, 0);
        body["next_action"] = json!({"redirect_to_url": {"url": "https://3ds.example"}});
        transport.push_response(HttpResponse::json(200, body));
        let stripe = gateway(transport);

        let response = stripe.fetch_transaction("pi_123").await.unwrap();
        assert_eq!(response.status, TransactionStatus::RequiresAction);
        assert_eq!(
            response.next_action,
            Some(NextAction::Redirect { url: "https://3ds.example".into() })
        );
    }

    #[tokio::test]
    async fn capture_and_refund_round_trip() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(200, intent("succeeded", 10_000, 0, 6_000)));
        transport.push_response(HttpResponse::json(
            200,
            json!({"id": "re_1", "amount": 2_000, "currency": "usd", "status": "succeeded"}),
        ));
        let stripe = gateway(transport.clone());

        let capture = stripe
            .capture(&CaptureRequest {
                idempotency_key: "cap-1".into(),
                transaction_id: "pi_123".into(),
                amount: usd(6_000),
                final_capture: true,
            })
            .await
            .unwrap();
        assert_eq!(capture.amount_captured, usd(6_000));
        assert_eq!(capture.capture_reference, "ch_123");
        assert!(transport.requests()[0].url.ends_with("/v1/payment_intents/pi_123/capture"));

        let refund = stripe
            .refund(&GatewayRefundRequest {
                idempotency_key: "ref-1".into(),
                transaction_id: "pi_123".into(),
                amount: usd(2_000),
                reason: RefundReason::RequestedByCustomer,
                reverse_transfers: true,
                refund_application_fee: true,
                metadata: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(refund.status, RefundStatus::Succeeded);
        let body = transport.last_request().unwrap().body_string();
        assert!(body.contains("reverse_transfer=true"));
        assert!(body.contains("reason=requested_by_customer"));
    }

    fn sign(secret: &str, payload: &[u8], timestamp: i64) -> String {
        let mut signed = timestamp.to_string().into_bytes();
        signed.push(b'.');
        signed.extend_from_slice(payload);
        format!("t={timestamp},v1={}", hmac_sha256_hex(&SecretString::new(secret), &signed))
    }

    #[tokio::test]
    async fn webhook_signatures_are_verified_against_the_raw_body() {
        let stripe = gateway(Arc::new(MockTransport::new()));
        let payload = json!({
            "id": "evt_1",
            "type": "payment_intent.succeeded",
            "created": Utc::now().timestamp(),
            "data": {"object": {"id": "pi_123", "amount": 10_000, "currency": "usd"}}
        })
        .to_string()
        .into_bytes();

        let now = Utc::now().timestamp();
        let headers = Headers::new().with("Stripe-Signature", sign("whsec_test", &payload, now));
        let event = stripe.parse_webhook(&payload, &headers).await.unwrap();
        assert_eq!(event.provider_event_id, "evt_1");
        assert_eq!(event.kind, GatewayEventKind::PaymentCaptured);
        assert_eq!(event.transaction_id.as_deref(), Some("pi_123"));
        assert_eq!(event.amount, Some(usd(10_000)));

        // Wrong secret.
        let bad = Headers::new().with("Stripe-Signature", sign("whsec_other", &payload, now));
        assert!(stripe.parse_webhook(&payload, &bad).await.is_err());

        // Replayed outside the tolerance window.
        let old = Headers::new()
            .with("Stripe-Signature", sign("whsec_test", &payload, now - 100_000));
        assert!(stripe.parse_webhook(&payload, &old).await.is_err());

        // Missing header.
        assert!(stripe.parse_webhook(&payload, &Headers::new()).await.is_err());
    }

    #[tokio::test]
    async fn dispute_events_carry_the_outcome() {
        let stripe = gateway(Arc::new(MockTransport::new()));
        let payload = json!({
            "id": "evt_2",
            "type": "charge.dispute.closed",
            "created": Utc::now().timestamp(),
            "data": {"object": {"id": "dp_1", "status": "won", "payment_intent": "pi_9"}}
        })
        .to_string()
        .into_bytes();
        let headers = Headers::new()
            .with("Stripe-Signature", sign("whsec_test", &payload, Utc::now().timestamp()));

        let event = stripe.parse_webhook(&payload, &headers).await.unwrap();
        assert_eq!(event.kind, GatewayEventKind::DisputeClosed { won: true });
    }

    #[test]
    fn decline_code_mapping_is_conservative() {
        assert_eq!(map_decline_code(Some("stolen_card")), DeclineCode::LostOrStolenCard);
        assert_eq!(map_decline_code(Some("authentication_required")), DeclineCode::AuthenticationRequired);
        assert_eq!(map_decline_code(Some("something_new")), DeclineCode::GenericDecline);
        assert_eq!(map_decline_code(None), DeclineCode::GenericDecline);
    }

    #[test]
    fn capabilities_reflect_stripe_limitations() {
        let stripe = gateway(Arc::new(MockTransport::new()));
        let capabilities = stripe.capabilities();
        assert!(capabilities.partial_capture);
        assert!(!capabilities.multi_capture, "Stripe closes the auth on first capture");
        assert!(capabilities.require(&STRIPE_GATEWAY_ID, "multi_capture").is_err());
    }
}
