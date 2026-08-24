//! A deterministic in-process gateway.
//!
//! `MockGateway` implements the whole [`PaymentGateway`] surface with real
//! bookkeeping: authorisations expire, captures are validated against the
//! remaining hold, refunds cannot exceed captures, and idempotency keys replay
//! the original response. That makes it useful well beyond unit tests — it is a
//! working sandbox for local development and integration environments.
//!
//! Behaviour is driven by the instrument token, so tests can trigger any
//! outcome without configuration:
//!
//! | token contains | outcome |
//! |---|---|
//! | `decline` | generic decline |
//! | `insufficient` | insufficient funds |
//! | `expired` | expired card |
//! | `3ds` / `authentication` | requires a 3-D Secure challenge |
//! | `error` | retryable gateway error |
//! | anything else | success |

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde_json::json;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::error::{DeclineCode, Error, Result};
use crate::gateway::{
    AuthorizeRequest, AuthorizeResponse, CancelRequest, Capabilities, CaptureMode, CaptureRequest,
    CaptureResponse, CustomerRequest, GatewayEvent, GatewayEventKind, GatewayId,
    GatewayRefundRequest, GatewayRefundResponse, InstrumentRef, NextAction, PaymentGateway,
    RefundStatus, TransactionStatus, VaultRequest,
};
use crate::money::Money;
use crate::payment::method::{CardSummary, PaymentMethodKind, PaymentMethodRef};
use crate::secret::SecretString;
use crate::webhook::{Headers, verify_hmac_sha256};

/// Identifier of the mock adapter.
pub const MOCK_GATEWAY_ID: GatewayId = GatewayId::from_static("mock");

#[derive(Debug, Clone)]
struct Transaction {
    id: String,
    authorized: Money,
    captured: Money,
    refunded: Money,
    status: TransactionStatus,
    expires_at: DateTime<Utc>,
}

/// An in-process payment gateway with full authorisation bookkeeping.
#[derive(Debug)]
pub struct MockGateway {
    id: GatewayId,
    capabilities: Capabilities,
    transactions: RwLock<HashMap<String, Transaction>>,
    idempotency: RwLock<HashMap<String, String>>,
    customers: RwLock<HashMap<String, String>>,
    counter: std::sync::atomic::AtomicU64,
    webhook_secret: SecretString,
}

impl Default for MockGateway {
    fn default() -> Self {
        Self::new()
    }
}

impl MockGateway {
    /// A gateway with every capability enabled.
    pub fn new() -> Self {
        Self {
            id: MOCK_GATEWAY_ID,
            capabilities: Capabilities::full(),
            transactions: RwLock::new(HashMap::new()),
            idempotency: RwLock::new(HashMap::new()),
            customers: RwLock::new(HashMap::new()),
            counter: std::sync::atomic::AtomicU64::new(1),
            webhook_secret: SecretString::new("whsec_mock"),
        }
    }

    /// A gateway with a custom id, for testing multi-gateway routing.
    pub fn with_id(id: GatewayId) -> Self {
        Self { id, ..Self::new() }
    }

    /// A gateway with restricted capabilities, to test capability errors.
    pub fn with_capabilities(capabilities: Capabilities) -> Self {
        Self { capabilities, ..Self::new() }
    }

    /// The secret used to sign this gateway's webhooks.
    pub fn webhook_secret(&self) -> &SecretString {
        &self.webhook_secret
    }

    /// Current state of a transaction, for assertions in tests.
    pub fn transaction_captured(&self, transaction_id: &str) -> Option<Money> {
        self.transactions
            .read()
            .ok()?
            .get(transaction_id)
            .map(|transaction| transaction.captured)
    }

    fn next_id(&self, prefix: &str) -> String {
        let value = self.counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        format!("{prefix}_{value:08}")
    }

    fn simulate(&self, instrument: &InstrumentRef) -> Result<Option<NextAction>> {
        let token = instrument.token().to_ascii_lowercase();
        if token.contains("insufficient") {
            return Err(Error::Declined {
                code: DeclineCode::InsufficientFunds,
                message: "the card has insufficient funds".to_owned(),
            });
        }
        if token.contains("expired") {
            return Err(Error::Declined {
                code: DeclineCode::ExpiredCard,
                message: "the card has expired".to_owned(),
            });
        }
        if token.contains("decline") {
            return Err(Error::Declined {
                code: DeclineCode::GenericDecline,
                message: "the card was declined".to_owned(),
            });
        }
        if token.contains("error") {
            return Err(Error::Gateway {
                gateway: self.id.to_string(),
                provider_code: Some("api_error".to_owned()),
                message: "the gateway is temporarily unavailable".to_owned(),
                retryable: true,
            });
        }
        if token.contains("3ds") || token.contains("authentication") {
            return Ok(Some(NextAction::Redirect {
                url: format!("https://mock-gateway.test/3ds/{}", self.next_id("acs")),
            }));
        }
        Ok(None)
    }

    fn cached_response(&self, key: &str) -> Result<Option<AuthorizeResponse>> {
        let cache = self
            .idempotency
            .read()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?;
        let Some(transaction_id) = cache.get(key) else {
            return Ok(None);
        };
        self.build_response(transaction_id).map(Some)
    }

    fn build_response(&self, transaction_id: &str) -> Result<AuthorizeResponse> {
        let transactions = self
            .transactions
            .read()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?;
        let transaction = transactions
            .get(transaction_id)
            .ok_or_else(|| Error::not_found("transaction", transaction_id))?;
        Ok(AuthorizeResponse {
            transaction_id: transaction.id.clone(),
            status: transaction.status,
            amount_authorized: transaction.authorized,
            amount_captured: transaction.captured,
            expires_at: Some(transaction.expires_at),
            next_action: None,
            processor_reference: Some(format!("mockproc_{}", transaction.id)),
            raw: Some(json!({ "object": "mock_transaction", "id": transaction.id })),
        })
    }

    /// Build a signed webhook payload, so integration tests can exercise the
    /// full verify → deduplicate → handle path.
    pub fn sign_webhook(&self, payload: &[u8]) -> Headers {
        Headers::new().with(
            "x-mock-signature",
            crate::webhook::hmac_sha256_hex(&self.webhook_secret, payload),
        )
    }

    /// Force an authorisation to look expired, for testing expiry handling.
    pub fn expire_authorization(&self, transaction_id: &str) -> Result<()> {
        let mut transactions = self
            .transactions
            .write()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?;
        let transaction = transactions
            .get_mut(transaction_id)
            .ok_or_else(|| Error::not_found("transaction", transaction_id))?;
        transaction.expires_at = Utc::now() - Duration::seconds(1);
        Ok(())
    }
}

#[async_trait]
impl PaymentGateway for MockGateway {
    fn id(&self) -> GatewayId {
        self.id.clone()
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    async fn upsert_customer(&self, request: &CustomerRequest) -> Result<String> {
        let mut customers = self
            .customers
            .write()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?;
        let key = request.customer_id.to_string();
        if let Some(existing) = customers.get(&key) {
            return Ok(existing.clone());
        }
        let id = self.next_id("cus");
        customers.insert(key, id.clone());
        Ok(id)
    }

    async fn vault_payment_method(&self, request: &VaultRequest) -> Result<PaymentMethodRef> {
        self.capabilities.require(&self.id, "stored_credentials")?;
        let card = CardSummary::new("visa", "4242", 12, Utc::now().year_plus(3))?;
        let mut method = PaymentMethodRef::new(
            request.customer_id.clone(),
            self.id.clone(),
            self.next_id("pm"),
            PaymentMethodKind::Card(card),
        )
        .with_gateway_customer(request.gateway_customer_id.clone());
        method.reusable = request.allow_off_session;
        method.metadata = request.metadata.clone();
        Ok(method)
    }

    async fn detach_payment_method(&self, _token: &str) -> Result<()> {
        Ok(())
    }

    async fn list_payment_methods(
        &self,
        _gateway_customer_id: &str,
    ) -> Result<Vec<PaymentMethodKind>> {
        Ok(vec![PaymentMethodKind::Card(CardSummary::new(
            "visa",
            "4242",
            12,
            Utc::now().year_plus(3),
        )?)])
    }

    async fn authorize(&self, request: &AuthorizeRequest) -> Result<AuthorizeResponse> {
        request.validate()?;
        if !self.capabilities.supports_currency(request.amount.currency()) {
            return Err(Error::configuration(format!(
                "gateway '{}' does not support {}",
                self.id,
                request.amount.currency()
            )));
        }
        if request.capture_mode == CaptureMode::Manual {
            self.capabilities.require(&self.id, "delayed_capture")?;
        }
        if !request.transfers.is_empty() {
            self.capabilities.require(&self.id, "connected_accounts")?;
        }
        if request.application_fee.is_some() {
            self.capabilities.require(&self.id, "application_fees")?;
        }

        // Replaying the same key must not charge twice.
        if let Some(cached) = self.cached_response(&request.idempotency_key)? {
            return Ok(cached);
        }

        if let Some(next_action) = self.simulate(&request.instrument)? {
            let transaction = Transaction {
                id: self.next_id("txn"),
                authorized: Money::zero(request.amount.currency()),
                captured: Money::zero(request.amount.currency()),
                refunded: Money::zero(request.amount.currency()),
                status: TransactionStatus::RequiresAction,
                expires_at: Utc::now() + self.capabilities.authorization_validity,
            };
            let id = transaction.id.clone();
            self.transactions
                .write()
                .map_err(|_| Error::internal("mock gateway lock poisoned"))?
                .insert(id.clone(), transaction);
            self.idempotency
                .write()
                .map_err(|_| Error::internal("mock gateway lock poisoned"))?
                .insert(request.idempotency_key.clone(), id.clone());
            let mut response = self.build_response(&id)?;
            response.next_action = Some(next_action);
            return Ok(response);
        }

        let automatic = request.capture_mode == CaptureMode::Automatic;
        let transaction = Transaction {
            id: self.next_id("txn"),
            authorized: request.amount,
            captured: if automatic {
                request.amount
            } else {
                Money::zero(request.amount.currency())
            },
            refunded: Money::zero(request.amount.currency()),
            status: if automatic {
                TransactionStatus::Captured
            } else {
                TransactionStatus::Authorized
            },
            expires_at: Utc::now() + self.capabilities.authorization_validity,
        };
        let id = transaction.id.clone();
        self.transactions
            .write()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?
            .insert(id.clone(), transaction);
        self.idempotency
            .write()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?
            .insert(request.idempotency_key.clone(), id.clone());
        self.build_response(&id)
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResponse> {
        let mut transactions = self
            .transactions
            .write()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?;
        let transaction = transactions
            .get_mut(&request.transaction_id)
            .ok_or_else(|| Error::not_found("transaction", &request.transaction_id))?;

        if transaction.status != TransactionStatus::Authorized
            && transaction.status != TransactionStatus::PartiallyCaptured
        {
            return Err(Error::InvalidTransition {
                kind: "transaction",
                id: transaction.id.clone(),
                from: format!("{:?}", transaction.status),
                to: "Captured".to_owned(),
            });
        }
        if Utc::now() >= transaction.expires_at {
            return Err(Error::Gateway {
                gateway: self.id.to_string(),
                provider_code: Some("authorization_expired".to_owned()),
                message: "the authorization has expired".to_owned(),
                retryable: false,
            });
        }
        let remaining = transaction.authorized.try_sub(transaction.captured)?;
        if request.amount > remaining {
            return Err(Error::validation(format!(
                "cannot capture {}: only {remaining} remains authorized",
                request.amount
            )));
        }
        if request.amount < remaining {
            self.capabilities.require(&self.id, "partial_capture")?;
        }
        if !transaction.captured.is_zero() {
            self.capabilities.require(&self.id, "multi_capture")?;
        }

        transaction.captured = transaction.captured.try_add(request.amount)?;
        let fully = transaction.captured == transaction.authorized || request.final_capture;
        transaction.status = if fully {
            if request.final_capture {
                transaction.authorized = transaction.captured;
            }
            TransactionStatus::Captured
        } else {
            TransactionStatus::PartiallyCaptured
        };

        Ok(CaptureResponse {
            capture_reference: format!("cap_{}", transaction.id),
            status: transaction.status,
            amount_captured: request.amount,
            total_captured: transaction.captured,
            raw: None,
        })
    }

    async fn cancel(&self, request: &CancelRequest) -> Result<AuthorizeResponse> {
        {
            let mut transactions = self
                .transactions
                .write()
                .map_err(|_| Error::internal("mock gateway lock poisoned"))?;
            let transaction = transactions
                .get_mut(&request.transaction_id)
                .ok_or_else(|| Error::not_found("transaction", &request.transaction_id))?;
            if transaction.captured.is_positive() {
                return Err(Error::validation(
                    "cannot void a transaction that has captured funds",
                ));
            }
            transaction.status = TransactionStatus::Canceled;
        }
        self.build_response(&request.transaction_id)
    }

    async fn refund(&self, request: &GatewayRefundRequest) -> Result<GatewayRefundResponse> {
        self.capabilities.require(&self.id, "refunds")?;
        let mut transactions = self
            .transactions
            .write()
            .map_err(|_| Error::internal("mock gateway lock poisoned"))?;
        let transaction = transactions
            .get_mut(&request.transaction_id)
            .ok_or_else(|| Error::not_found("transaction", &request.transaction_id))?;

        let refundable = transaction.captured.try_sub(transaction.refunded)?;
        if request.amount > refundable {
            return Err(Error::validation(format!(
                "cannot refund {}: only {refundable} is refundable",
                request.amount
            )));
        }
        if request.amount < refundable {
            self.capabilities.require(&self.id, "partial_refunds")?;
        }
        transaction.refunded = transaction.refunded.try_add(request.amount)?;

        Ok(GatewayRefundResponse {
            refund_reference: format!("re_{}_{}", transaction.id, transaction.refunded.minor()),
            amount: request.amount,
            status: RefundStatus::Succeeded,
            raw: None,
        })
    }

    async fn fetch_transaction(&self, transaction_id: &str) -> Result<AuthorizeResponse> {
        self.build_response(transaction_id)
    }

    async fn parse_webhook(&self, payload: &[u8], headers: &Headers) -> Result<GatewayEvent> {
        verify_hmac_sha256(&self.webhook_secret, payload, headers.require("x-mock-signature")?)?;
        let body: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|error| Error::WebhookVerification(format!("invalid JSON body: {error}")))?;

        let provider_type = body["type"].as_str().unwrap_or("unknown").to_owned();
        let kind = match provider_type.as_str() {
            "payment.authorized" => GatewayEventKind::PaymentAuthorized,
            "payment.captured" => GatewayEventKind::PaymentCaptured,
            "payment.failed" => GatewayEventKind::PaymentFailed,
            "payment.canceled" => GatewayEventKind::PaymentCanceled,
            "refund.succeeded" => GatewayEventKind::RefundSucceeded,
            "dispute.opened" => GatewayEventKind::DisputeOpened,
            other => GatewayEventKind::Unknown { provider_type: other.to_owned() },
        };

        Ok(GatewayEvent {
            provider_event_id: body["id"].as_str().unwrap_or_default().to_owned(),
            gateway: self.id.clone(),
            kind,
            transaction_id: body["data"]["transaction_id"].as_str().map(str::to_owned),
            amount: None,
            occurred_at: Utc::now(),
            raw: Some(body),
        })
    }
}

/// Small helper so the mock does not vault cards that are already expired.
trait YearPlus {
    fn year_plus(&self, years: i32) -> i32;
}

impl YearPlus for DateTime<Utc> {
    fn year_plus(&self, years: i32) -> i32 {
        use chrono::Datelike;
        self.year() + years
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn request(key: &str, token: &str, amount: i64) -> AuthorizeRequest {
        AuthorizeRequest::new(
            key,
            usd(amount),
            InstrumentRef::SingleUseToken { token: token.to_owned() },
        )
    }

    #[tokio::test]
    async fn automatic_capture_takes_the_money_immediately() {
        let gateway = MockGateway::new();
        let response = gateway.authorize(&request("k1", "tok_visa", 10_000)).await.unwrap();
        assert_eq!(response.status, TransactionStatus::Captured);
        assert_eq!(response.amount_captured, usd(10_000));
    }

    #[tokio::test]
    async fn manual_capture_holds_then_captures_in_parts() {
        let gateway = MockGateway::new();
        let authorization = gateway
            .authorize(&request("k1", "tok_visa", 10_000).manual_capture())
            .await
            .unwrap();
        assert_eq!(authorization.status, TransactionStatus::Authorized);
        assert!(authorization.amount_captured.is_zero());

        let first = gateway
            .capture(&CaptureRequest {
                idempotency_key: "c1".into(),
                transaction_id: authorization.transaction_id.clone(),
                amount: usd(4_000),
                final_capture: false,
            })
            .await
            .unwrap();
        assert_eq!(first.status, TransactionStatus::PartiallyCaptured);

        let second = gateway
            .capture(&CaptureRequest {
                idempotency_key: "c2".into(),
                transaction_id: authorization.transaction_id.clone(),
                amount: usd(6_000),
                final_capture: false,
            })
            .await
            .unwrap();
        assert_eq!(second.status, TransactionStatus::Captured);
        assert_eq!(second.total_captured, usd(10_000));

        // Nothing left to take.
        assert!(
            gateway
                .capture(&CaptureRequest {
                    idempotency_key: "c3".into(),
                    transaction_id: authorization.transaction_id,
                    amount: usd(1),
                    final_capture: false,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn final_capture_releases_the_remainder() {
        let gateway = MockGateway::new();
        let authorization = gateway
            .authorize(&request("k1", "tok_visa", 10_000).manual_capture())
            .await
            .unwrap();
        gateway
            .capture(&CaptureRequest {
                idempotency_key: "c1".into(),
                transaction_id: authorization.transaction_id.clone(),
                amount: usd(6_000),
                final_capture: true,
            })
            .await
            .unwrap();
        let state = gateway.fetch_transaction(&authorization.transaction_id).await.unwrap();
        assert_eq!(state.status, TransactionStatus::Captured);
        assert_eq!(state.amount_authorized, usd(6_000));
    }

    #[tokio::test]
    async fn idempotency_keys_never_charge_twice() {
        let gateway = MockGateway::new();
        let first = gateway.authorize(&request("same-key", "tok_visa", 5_000)).await.unwrap();
        let second = gateway.authorize(&request("same-key", "tok_visa", 5_000)).await.unwrap();
        assert_eq!(first.transaction_id, second.transaction_id);
    }

    #[tokio::test]
    async fn declines_are_normalised() {
        let gateway = MockGateway::new();
        let error = gateway.authorize(&request("k1", "tok_insufficient", 100)).await.unwrap_err();
        assert!(matches!(
            error,
            Error::Declined { code: DeclineCode::InsufficientFunds, .. }
        ));

        let error = gateway.authorize(&request("k2", "tok_decline", 100)).await.unwrap_err();
        assert!(matches!(error, Error::Declined { code: DeclineCode::GenericDecline, .. }));

        let error = gateway.authorize(&request("k3", "tok_error", 100)).await.unwrap_err();
        assert!(error.is_retryable());
    }

    #[tokio::test]
    async fn three_d_secure_returns_a_next_action() {
        let gateway = MockGateway::new();
        let response = gateway.authorize(&request("k1", "tok_3ds", 100)).await.unwrap();
        assert_eq!(response.status, TransactionStatus::RequiresAction);
        assert!(matches!(response.next_action, Some(NextAction::Redirect { .. })));
    }

    #[tokio::test]
    async fn expired_authorizations_cannot_be_captured() {
        let gateway = MockGateway::new();
        let authorization = gateway
            .authorize(&request("k1", "tok_visa", 1_000).manual_capture())
            .await
            .unwrap();
        gateway.expire_authorization(&authorization.transaction_id).unwrap();

        let error = gateway
            .capture(&CaptureRequest {
                idempotency_key: "c1".into(),
                transaction_id: authorization.transaction_id,
                amount: usd(1_000),
                final_capture: false,
            })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Gateway { .. }));
    }

    #[tokio::test]
    async fn refunds_are_bounded_by_captures() {
        let gateway = MockGateway::new();
        let authorization = gateway.authorize(&request("k1", "tok_visa", 10_000)).await.unwrap();
        let refund = gateway
            .refund(&GatewayRefundRequest {
                idempotency_key: "r1".into(),
                transaction_id: authorization.transaction_id.clone(),
                amount: usd(4_000),
                reason: Default::default(),
                reverse_transfers: true,
                refund_application_fee: true,
                metadata: Default::default(),
            })
            .await
            .unwrap();
        assert_eq!(refund.status, RefundStatus::Succeeded);

        let error = gateway
            .refund(&GatewayRefundRequest {
                idempotency_key: "r2".into(),
                transaction_id: authorization.transaction_id,
                amount: usd(6_001),
                reason: Default::default(),
                reverse_transfers: true,
                refund_application_fee: true,
                metadata: Default::default(),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Validation(_)));
    }

    #[tokio::test]
    async fn voiding_a_captured_transaction_is_refused() {
        let gateway = MockGateway::new();
        let authorization = gateway.authorize(&request("k1", "tok_visa", 1_000)).await.unwrap();
        assert!(
            gateway
                .cancel(&CancelRequest {
                    idempotency_key: "v1".into(),
                    transaction_id: authorization.transaction_id,
                    reason: None,
                })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn missing_capabilities_are_reported_before_charging() {
        let mut capabilities = Capabilities::full();
        capabilities.delayed_capture = false;
        let gateway = MockGateway::with_capabilities(capabilities);

        let error = gateway
            .authorize(&request("k1", "tok_visa", 1_000).manual_capture())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::UnsupportedCapability { .. }));
    }

    #[tokio::test]
    async fn webhooks_are_verified_and_normalised() {
        let gateway = MockGateway::new();
        let payload = json!({
            "id": "evt_1",
            "type": "payment.captured",
            "data": { "transaction_id": "txn_00000001" }
        })
        .to_string()
        .into_bytes();

        let headers = gateway.sign_webhook(&payload);
        let event = gateway.parse_webhook(&payload, &headers).await.unwrap();
        assert_eq!(event.provider_event_id, "evt_1");
        assert_eq!(event.kind, GatewayEventKind::PaymentCaptured);
        assert_eq!(event.transaction_id.as_deref(), Some("txn_00000001"));

        // Tampering invalidates the signature.
        let tampered = json!({"id": "evt_2", "type": "payment.captured"}).to_string().into_bytes();
        assert!(gateway.parse_webhook(&tampered, &headers).await.is_err());
    }

    #[tokio::test]
    async fn customers_are_created_once() {
        let gateway = MockGateway::new();
        let request = CustomerRequest {
            customer_id: crate::ids::CustomerId::from_string("cus_local"),
            email: Some("a@example.com".into()),
            name: None,
            metadata: Default::default(),
        };
        let first = gateway.upsert_customer(&request).await.unwrap();
        let second = gateway.upsert_customer(&request).await.unwrap();
        assert_eq!(first, second);
    }
}
