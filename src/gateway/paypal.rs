//! PayPal adapter (Orders v2 + Payments v2).
//!
//! Enabled with the `paypal` feature. Like the Stripe adapter it is built on
//! [`HttpTransport`], so it carries no HTTP client of its own.
//!
//! PayPal differs from card processors in ways the abstraction has to absorb:
//!
//! * Amounts are decimal strings with the currency's own scale, not minor
//!   units, so every amount is converted through [`Money::to_decimal_string`].
//! * An order must be *approved by the buyer* before it can be captured, so
//!   `authorize` returns [`NextAction::Redirect`] to the approval link.
//! * Splits are expressed as `purchase_units`, one per recipient, each with its
//!   own `payee` and `platform_fees`.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::error::{DeclineCode, Error, Result};
use crate::gateway::http::{HttpRequest, HttpResponse, HttpTransport};
use crate::gateway::{
    AuthorizeRequest, AuthorizeResponse, CancelRequest, Capabilities, CaptureMode, CaptureRequest,
    CaptureResponse, GatewayEvent, GatewayEventKind, GatewayId, GatewayRefundRequest,
    GatewayRefundResponse, NextAction, PaymentGateway, RefundStatus, TransactionStatus,
};
use crate::money::{Currency, Money};
use crate::secret::SecretString;
use crate::webhook::Headers;

/// Identifier of the PayPal adapter.
pub const PAYPAL_GATEWAY_ID: GatewayId = GatewayId::from_static("paypal");

/// PayPal's Orders v2 adapter.
pub struct PayPalGateway {
    transport: Arc<dyn HttpTransport>,
    access_token: SecretString,
    base_url: String,
    webhook_id: Option<String>,
    brand_name: Option<String>,
}

impl std::fmt::Debug for PayPalGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PayPalGateway").field("base_url", &self.base_url).finish_non_exhaustive()
    }
}

impl PayPalGateway {
    /// Build an adapter from a transport and an OAuth access token.
    ///
    /// Token refresh is the caller's concern: wrap it in your transport, where
    /// it can be shared with the rest of your PayPal integration.
    pub fn new(transport: Arc<dyn HttpTransport>, access_token: SecretString) -> Self {
        Self {
            transport,
            access_token,
            base_url: "https://api-m.paypal.com".to_owned(),
            webhook_id: None,
            brand_name: None,
        }
    }

    /// Builder: point at the sandbox or a mock.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Builder: set the webhook id used to verify notifications.
    pub fn with_webhook_id(mut self, webhook_id: impl Into<String>) -> Self {
        self.webhook_id = Some(webhook_id.into());
        self
    }

    /// Builder: set the brand shown on the approval page.
    pub fn with_brand_name(mut self, brand_name: impl Into<String>) -> Self {
        self.brand_name = Some(brand_name.into());
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn request(&self, request: HttpRequest, idempotency_key: Option<&str>) -> HttpRequest {
        let mut request = request.with_bearer_auth(&self.access_token);
        if let Some(key) = idempotency_key {
            // PayPal calls the idempotency header "PayPal-Request-Id".
            request = request.with_header("paypal-request-id", key);
        }
        request
    }

    async fn send(&self, request: HttpRequest) -> Result<Value> {
        let response = self.transport.execute(request).await?;
        self.interpret(response)
    }

    fn interpret(&self, response: HttpResponse) -> Result<Value> {
        if response.body.is_empty() && response.is_success() {
            return Ok(Value::Null);
        }
        let body = response.json_body().unwrap_or(Value::Null);
        if response.is_success() {
            return Ok(body);
        }

        let name = body["name"].as_str().unwrap_or("UNKNOWN");
        let issue = body["details"][0]["issue"].as_str();
        let message = body["details"][0]["description"]
            .as_str()
            .or_else(|| body["message"].as_str())
            .unwrap_or("PayPal returned an error")
            .to_owned();

        if let Some(code) = map_decline(issue.unwrap_or(name)) {
            return Err(Error::Declined { code, message });
        }
        Err(Error::Gateway {
            gateway: PAYPAL_GATEWAY_ID.to_string(),
            provider_code: Some(issue.unwrap_or(name).to_owned()),
            message,
            retryable: response.is_retryable(),
        })
    }

    fn amount_object(amount: Money) -> Value {
        json!({
            "currency_code": amount.currency().code(),
            "value": amount.to_decimal_string(),
        })
    }

    fn parse_order(&self, order: &Value) -> Result<AuthorizeResponse> {
        let status = order["status"].as_str().unwrap_or_default();
        let unit = &order["purchase_units"][0];
        let currency = Currency::from_code(
            unit["amount"]["currency_code"].as_str().unwrap_or("USD"),
        )?;
        let total = parse_amount(&unit["amount"], currency)?;

        let captures = &unit["payments"]["captures"];
        let authorizations = &unit["payments"]["authorizations"];

        let captured = sum_amounts(captures, currency, "COMPLETED")?;
        let authorized = sum_amounts(authorizations, currency, "CREATED")?;

        let transaction_status = match status {
            "COMPLETED" => TransactionStatus::Captured,
            "CREATED" | "SAVED" | "PAYER_ACTION_REQUIRED" | "APPROVED" => {
                TransactionStatus::RequiresAction
            }
            "VOIDED" => TransactionStatus::Canceled,
            _ if !authorized.is_zero() => TransactionStatus::Authorized,
            _ => TransactionStatus::Pending,
        };

        let next_action = order["links"]
            .as_array()
            .and_then(|links| {
                links.iter().find(|link| link["rel"].as_str() == Some("approve"))
            })
            .and_then(|link| link["href"].as_str())
            .map(|url| NextAction::Redirect { url: url.to_owned() });

        let amount_authorized = if !authorized.is_zero() {
            authorized
        } else if transaction_status == TransactionStatus::Captured {
            total
        } else {
            Money::zero(currency)
        };

        Ok(AuthorizeResponse {
            transaction_id: order["id"].as_str().unwrap_or_default().to_owned(),
            status: transaction_status,
            amount_authorized,
            amount_captured: captured,
            // PayPal honours an authorisation for 29 days, with a 3-day
            // guaranteed window.
            expires_at: Some(Utc::now() + chrono::Duration::days(29)),
            next_action: if transaction_status == TransactionStatus::RequiresAction {
                next_action
            } else {
                None
            },
            processor_reference: captures[0]["id"]
                .as_str()
                .or_else(|| authorizations[0]["id"].as_str())
                .map(str::to_owned),
            raw: Some(order.clone()),
        })
    }
}

fn parse_amount(amount: &Value, currency: Currency) -> Result<Money> {
    match amount["value"].as_str() {
        Some(value) => Money::parse_decimal(value, currency),
        None => Ok(Money::zero(currency)),
    }
}

fn sum_amounts(entries: &Value, currency: Currency, status: &str) -> Result<Money> {
    let Some(entries) = entries.as_array() else {
        return Ok(Money::zero(currency));
    };
    let mut total = Money::zero(currency);
    for entry in entries {
        if entry["status"].as_str().is_some_and(|value| value != status) {
            continue;
        }
        total = total.try_add(parse_amount(&entry["amount"], currency)?)?;
    }
    Ok(total)
}

fn map_decline(issue: &str) -> Option<DeclineCode> {
    Some(match issue {
        "INSTRUMENT_DECLINED" | "PAYER_CANNOT_PAY" | "TRANSACTION_REFUSED" => {
            DeclineCode::GenericDecline
        }
        "INSUFFICIENT_FUNDS" => DeclineCode::InsufficientFunds,
        "CARD_EXPIRED" => DeclineCode::ExpiredCard,
        "PAYER_ACTION_REQUIRED" | "PAYMENT_DENIED" => DeclineCode::AuthenticationRequired,
        "CURRENCY_NOT_SUPPORTED" | "INSTRUMENT_NOT_SUPPORTED" => DeclineCode::Unsupported,
        _ => return None,
    })
}

#[async_trait]
impl PaymentGateway for PayPalGateway {
    fn id(&self) -> GatewayId {
        PAYPAL_GATEWAY_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            delayed_capture: true,
            partial_capture: true,
            multi_capture: true,
            refunds: true,
            partial_refunds: true,
            connected_accounts: true,
            application_fees: true,
            // Instruments are vaulted through PayPal's separate Vault API.
            stored_credentials: false,
            webhooks: true,
            disputes: true,
            // Buyer authentication happens on PayPal's own approval page.
            three_d_secure: false,
            authorization_validity: chrono::Duration::days(29),
            currencies: Default::default(),
        }
    }

    async fn authorize(&self, request: &AuthorizeRequest) -> Result<AuthorizeResponse> {
        request.validate()?;

        // One purchase unit per recipient, so PayPal performs the split itself.
        let purchase_units: Vec<Value> = if request.transfers.is_empty() {
            vec![json!({
                "reference_id": request.order_id.as_ref().map(|id| id.to_string()).unwrap_or_else(|| "default".to_owned()),
                "amount": Self::amount_object(request.amount),
                "custom_id": request.order_id.as_ref().map(|id| id.to_string()),
                "description": request.description,
            })]
        } else {
            request
                .transfers
                .iter()
                .enumerate()
                .map(|(index, transfer)| {
                    let mut unit = json!({
                        "reference_id": format!("unit-{index}"),
                        "amount": Self::amount_object(transfer.amount),
                        "payee": {"merchant_id": transfer.destination.to_string()},
                        "description": transfer.description,
                    });
                    if let Some(fee) = request.application_fee
                        && index == 0
                        && fee.is_positive()
                    {
                        unit["payment_instruction"] = json!({
                            "disbursement_mode": "INSTANT",
                            "platform_fees": [{"amount": Self::amount_object(fee)}],
                        });
                    }
                    unit
                })
                .collect()
        };

        let mut body = json!({
            "intent": match request.capture_mode {
                CaptureMode::Automatic => "CAPTURE",
                CaptureMode::Manual => "AUTHORIZE",
            },
            "purchase_units": purchase_units,
        });
        if self.brand_name.is_some() || request.return_url.is_some() {
            body["payment_source"] = json!({
                "paypal": {
                    "experience_context": {
                        "brand_name": self.brand_name,
                        "return_url": request.return_url,
                        "user_action": "PAY_NOW",
                    }
                }
            });
        }

        let http = self.request(
            HttpRequest::post_json(self.url("/v2/checkout/orders"), &body)?,
            Some(&request.idempotency_key),
        );
        let order = self.send(http).await?;
        self.parse_order(&order)
    }

    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResponse> {
        // PayPal captures an *authorization*, not the order, so the caller
        // passes the authorization id it received from `authorize`.
        let body = json!({
            "amount": Self::amount_object(request.amount),
            "final_capture": request.final_capture,
        });
        let http = self.request(
            HttpRequest::post_json(
                self.url(&format!(
                    "/v2/payments/authorizations/{}/capture",
                    request.transaction_id
                )),
                &body,
            )?,
            Some(&request.idempotency_key),
        );
        let capture = self.send(http).await?;

        let currency =
            Currency::from_code(capture["amount"]["currency_code"].as_str().unwrap_or("USD"))?;
        let amount = parse_amount(&capture["amount"], currency)?;
        Ok(CaptureResponse {
            capture_reference: capture["id"].as_str().unwrap_or_default().to_owned(),
            status: if request.final_capture {
                TransactionStatus::Captured
            } else {
                TransactionStatus::PartiallyCaptured
            },
            amount_captured: amount,
            total_captured: amount,
            raw: Some(capture),
        })
    }

    async fn cancel(&self, request: &CancelRequest) -> Result<AuthorizeResponse> {
        let http = self.request(
            HttpRequest::post_json(
                self.url(&format!("/v2/payments/authorizations/{}/void", request.transaction_id)),
                &json!({}),
            )?,
            Some(&request.idempotency_key),
        );
        self.send(http).await?;

        Ok(AuthorizeResponse {
            transaction_id: request.transaction_id.clone(),
            status: TransactionStatus::Canceled,
            amount_authorized: Money::zero(Currency::USD),
            amount_captured: Money::zero(Currency::USD),
            expires_at: None,
            next_action: None,
            processor_reference: None,
            raw: None,
        })
    }

    async fn refund(&self, request: &GatewayRefundRequest) -> Result<GatewayRefundResponse> {
        let body = json!({
            "amount": Self::amount_object(request.amount),
            "note_to_payer": format!("Refund: {:?}", request.reason),
        });
        let http = self.request(
            HttpRequest::post_json(
                self.url(&format!("/v2/payments/captures/{}/refund", request.transaction_id)),
                &body,
            )?,
            Some(&request.idempotency_key),
        );
        let refund = self.send(http).await?;

        let currency =
            Currency::from_code(refund["amount"]["currency_code"].as_str().unwrap_or("USD"))?;
        Ok(GatewayRefundResponse {
            refund_reference: refund["id"].as_str().unwrap_or_default().to_owned(),
            amount: parse_amount(&refund["amount"], currency)?,
            status: match refund["status"].as_str().unwrap_or_default() {
                "COMPLETED" => RefundStatus::Succeeded,
                "PENDING" => RefundStatus::Pending,
                "CANCELLED" => RefundStatus::Canceled,
                _ => RefundStatus::Failed,
            },
            raw: Some(refund),
        })
    }

    async fn fetch_transaction(&self, transaction_id: &str) -> Result<AuthorizeResponse> {
        let http = self.request(
            HttpRequest::get(self.url(&format!("/v2/checkout/orders/{transaction_id}"))),
            None,
        );
        let order = self.send(http).await?;
        self.parse_order(&order)
    }

    async fn parse_webhook(&self, payload: &[u8], headers: &Headers) -> Result<GatewayEvent> {
        let webhook_id = self
            .webhook_id
            .as_ref()
            .ok_or_else(|| Error::configuration("no PayPal webhook id configured"))?;

        let body: Value = serde_json::from_slice(payload)
            .map_err(|error| Error::WebhookVerification(format!("invalid JSON body: {error}")))?;

        // PayPal signs with a certificate chain rather than a shared secret, so
        // verification is delegated to their API. The raw body is forwarded
        // untouched, which is what the signature covers.
        let verification = json!({
            "auth_algo": headers.require("paypal-auth-algo")?,
            "cert_url": headers.require("paypal-cert-url")?,
            "transmission_id": headers.require("paypal-transmission-id")?,
            "transmission_sig": headers.require("paypal-transmission-sig")?,
            "transmission_time": headers.require("paypal-transmission-time")?,
            "webhook_id": webhook_id,
            "webhook_event": body,
        });
        let http = self.request(
            HttpRequest::post_json(
                self.url("/v1/notifications/verify-webhook-signature"),
                &verification,
            )?,
            None,
        );
        let result = self.send(http).await?;
        if result["verification_status"].as_str() != Some("SUCCESS") {
            return Err(Error::WebhookVerification(
                "PayPal reported the webhook signature as invalid".to_owned(),
            ));
        }

        let event_type = body["event_type"].as_str().unwrap_or("unknown");
        let resource = &body["resource"];
        let kind = match event_type {
            "PAYMENT.AUTHORIZATION.CREATED" => GatewayEventKind::PaymentAuthorized,
            "PAYMENT.CAPTURE.COMPLETED" | "CHECKOUT.ORDER.COMPLETED" => {
                GatewayEventKind::PaymentCaptured
            }
            "PAYMENT.CAPTURE.DENIED" => GatewayEventKind::PaymentFailed,
            "PAYMENT.AUTHORIZATION.VOIDED" => GatewayEventKind::PaymentCanceled,
            "PAYMENT.CAPTURE.REFUNDED" => GatewayEventKind::RefundSucceeded,
            "CUSTOMER.DISPUTE.CREATED" => GatewayEventKind::DisputeOpened,
            "CUSTOMER.DISPUTE.RESOLVED" => GatewayEventKind::DisputeClosed {
                won: resource["dispute_outcome"]["outcome_code"].as_str()
                    == Some("RESOLVED_SELLER_FAVOUR"),
            },
            "PAYMENT.PAYOUTSBATCH.SUCCESS" => GatewayEventKind::PayoutPaid,
            other => GatewayEventKind::Unknown { provider_type: other.to_owned() },
        };

        let amount = match Currency::from_code(
            resource["amount"]["currency_code"].as_str().unwrap_or("USD"),
        ) {
            Ok(currency) => parse_amount(&resource["amount"], currency).ok(),
            Err(_) => None,
        };

        let occurred_at: DateTime<Utc> = body["create_time"]
            .as_str()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        Ok(GatewayEvent {
            provider_event_id: body["id"].as_str().unwrap_or_default().to_owned(),
            gateway: self.id(),
            kind,
            transaction_id: resource["id"].as_str().map(str::to_owned),
            amount,
            occurred_at,
            raw: Some(body),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::TransferInstruction;
    use crate::gateway::http::MockTransport;
    use crate::ids::{AccountId, OrderId};

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn gateway(transport: Arc<MockTransport>) -> PayPalGateway {
        PayPalGateway::new(transport, SecretString::new("A21AA..."))
            .with_base_url("https://api-m.sandbox.paypal.com")
            .with_webhook_id("WH-123")
    }

    #[tokio::test]
    async fn amounts_are_sent_as_decimal_strings() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            201,
            json!({
                "id": "5O190127TN364715T",
                "status": "CREATED",
                "purchase_units": [{"amount": {"currency_code": "USD", "value": "123.45"}}],
                "links": [{"rel": "approve", "href": "https://paypal.test/approve"}]
            }),
        ));
        let paypal = gateway(transport.clone());

        let response = paypal
            .authorize(
                &AuthorizeRequest::new(
                    "idem-1",
                    usd(12_345),
                    crate::gateway::InstrumentRef::SingleUseToken { token: "n/a".into() },
                )
                .for_order(OrderId::from_string("ord_1")),
            )
            .await
            .unwrap();

        assert_eq!(response.status, TransactionStatus::RequiresAction);
        assert_eq!(
            response.next_action,
            Some(NextAction::Redirect { url: "https://paypal.test/approve".into() })
        );

        let sent = transport.last_request().unwrap();
        assert_eq!(sent.headers.get("paypal-request-id").unwrap(), "idem-1");
        let body: Value = serde_json::from_slice(sent.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["intent"], "CAPTURE");
        // Decimal string, not minor units.
        assert_eq!(body["purchase_units"][0]["amount"]["value"], "123.45");
    }

    #[tokio::test]
    async fn zero_decimal_currencies_are_not_scaled() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            201,
            json!({
                "id": "o1",
                "status": "CREATED",
                "purchase_units": [{"amount": {"currency_code": "JPY", "value": "1200"}}]
            }),
        ));
        let paypal = gateway(transport.clone());
        paypal
            .authorize(&AuthorizeRequest::new(
                "idem-1",
                Money::from_minor(1_200, Currency::JPY),
                crate::gateway::InstrumentRef::SingleUseToken { token: "n/a".into() },
            ))
            .await
            .unwrap();

        let body: Value =
            serde_json::from_slice(transport.last_request().unwrap().body.as_ref().unwrap())
                .unwrap();
        assert_eq!(body["purchase_units"][0]["amount"]["value"], "1200");
    }

    #[tokio::test]
    async fn splits_become_one_purchase_unit_per_payee() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            201,
            json!({
                "id": "o1",
                "status": "CREATED",
                "purchase_units": [{"amount": {"currency_code": "USD", "value": "100.00"}}]
            }),
        ));
        let paypal = gateway(transport.clone());

        paypal
            .authorize(
                &AuthorizeRequest::new(
                    "idem-1",
                    usd(10_000),
                    crate::gateway::InstrumentRef::SingleUseToken { token: "n/a".into() },
                )
                .with_transfers(vec![
                    TransferInstruction {
                        destination: AccountId::from_string("MERCHANT_A"),
                        amount: usd(6_000),
                        description: Some("Shop A".into()),
                        transfer_group: None,
                    },
                    TransferInstruction {
                        destination: AccountId::from_string("MERCHANT_B"),
                        amount: usd(3_500),
                        description: Some("Shop B".into()),
                        transfer_group: None,
                    },
                ])
                .with_application_fee(usd(500)),
            )
            .await
            .unwrap();

        let body: Value =
            serde_json::from_slice(transport.last_request().unwrap().body.as_ref().unwrap())
                .unwrap();
        let units = body["purchase_units"].as_array().unwrap();
        assert_eq!(units.len(), 2);
        assert_eq!(units[0]["payee"]["merchant_id"], "MERCHANT_A");
        assert_eq!(units[0]["amount"]["value"], "60.00");
        assert_eq!(units[1]["amount"]["value"], "35.00");
        assert_eq!(
            units[0]["payment_instruction"]["platform_fees"][0]["amount"]["value"],
            "5.00"
        );
    }

    #[tokio::test]
    async fn instrument_declined_is_normalised() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            422,
            json!({
                "name": "UNPROCESSABLE_ENTITY",
                "details": [{"issue": "INSTRUMENT_DECLINED", "description": "The instrument was declined."}]
            }),
        ));
        let paypal = gateway(transport);

        let error = paypal
            .authorize(&AuthorizeRequest::new(
                "idem-1",
                usd(100),
                crate::gateway::InstrumentRef::SingleUseToken { token: "n/a".into() },
            ))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Declined { code: DeclineCode::GenericDecline, .. }));
    }

    #[tokio::test]
    async fn captured_orders_report_their_totals() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            200,
            json!({
                "id": "o1",
                "status": "COMPLETED",
                "purchase_units": [{
                    "amount": {"currency_code": "USD", "value": "100.00"},
                    "payments": {"captures": [
                        {"id": "cap_1", "status": "COMPLETED", "amount": {"currency_code": "USD", "value": "60.00"}},
                        {"id": "cap_2", "status": "COMPLETED", "amount": {"currency_code": "USD", "value": "40.00"}}
                    ]}
                }]
            }),
        ));
        let paypal = gateway(transport);

        let response = paypal.fetch_transaction("o1").await.unwrap();
        assert_eq!(response.status, TransactionStatus::Captured);
        assert_eq!(response.amount_captured, usd(10_000));
        assert_eq!(response.processor_reference.as_deref(), Some("cap_1"));
    }

    #[tokio::test]
    async fn refunds_post_to_the_capture() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            201,
            json!({"id": "re_1", "status": "COMPLETED", "amount": {"currency_code": "USD", "value": "25.00"}}),
        ));
        let paypal = gateway(transport.clone());

        let refund = paypal
            .refund(&GatewayRefundRequest {
                idempotency_key: "ref-1".into(),
                transaction_id: "cap_1".into(),
                amount: usd(2_500),
                reason: Default::default(),
                reverse_transfers: false,
                refund_application_fee: false,
                metadata: Default::default(),
            })
            .await
            .unwrap();

        assert_eq!(refund.amount, usd(2_500));
        assert_eq!(refund.status, RefundStatus::Succeeded);
        assert!(transport.last_request().unwrap().url.ends_with("/v2/payments/captures/cap_1/refund"));
    }

    #[tokio::test]
    async fn webhooks_are_verified_through_paypals_api() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            200,
            json!({"verification_status": "SUCCESS"}),
        ));
        let paypal = gateway(transport);

        let payload = json!({
            "id": "WH-EVT-1",
            "event_type": "PAYMENT.CAPTURE.COMPLETED",
            "create_time": "2024-01-01T00:00:00Z",
            "resource": {"id": "cap_1", "amount": {"currency_code": "USD", "value": "10.00"}}
        })
        .to_string()
        .into_bytes();

        let headers = Headers::new()
            .with("paypal-auth-algo", "SHA256withRSA")
            .with("paypal-cert-url", "https://api.paypal.com/cert")
            .with("paypal-transmission-id", "tx-1")
            .with("paypal-transmission-sig", "sig")
            .with("paypal-transmission-time", "2024-01-01T00:00:00Z");

        let event = paypal.parse_webhook(&payload, &headers).await.unwrap();
        assert_eq!(event.provider_event_id, "WH-EVT-1");
        assert_eq!(event.kind, GatewayEventKind::PaymentCaptured);
        assert_eq!(event.amount, Some(usd(1_000)));
    }

    #[tokio::test]
    async fn a_failed_verification_is_rejected() {
        let transport = Arc::new(MockTransport::new());
        transport.push_response(HttpResponse::json(
            200,
            json!({"verification_status": "FAILURE"}),
        ));
        let paypal = gateway(transport);

        let headers = Headers::new()
            .with("paypal-auth-algo", "SHA256withRSA")
            .with("paypal-cert-url", "https://api.paypal.com/cert")
            .with("paypal-transmission-id", "tx-1")
            .with("paypal-transmission-sig", "sig")
            .with("paypal-transmission-time", "2024-01-01T00:00:00Z");

        let error = paypal.parse_webhook(b"{\"id\":\"x\"}", &headers).await.unwrap_err();
        assert!(matches!(error, Error::WebhookVerification(_)));
    }

    #[test]
    fn capabilities_reflect_paypal_limitations() {
        let paypal = gateway(Arc::new(MockTransport::new()));
        let capabilities = paypal.capabilities();
        assert!(capabilities.multi_capture);
        assert!(!capabilities.stored_credentials);
        assert!(capabilities.require(&PAYPAL_GATEWAY_ID, "stored_credentials").is_err());
    }
}
