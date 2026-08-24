//! Webhook ingestion: one interface for every gateway's callbacks.
//!
//! Handling provider callbacks correctly is mostly about three things, and this
//! module makes all three hard to get wrong:
//!
//! 1. **Verify before you trust.** [`verify_hmac_sha256`] does a constant-time
//!    comparison over the *raw* body. Never parse first and verify later.
//! 2. **Deduplicate.** Providers retry aggressively and deliver out of order.
//!    [`WebhookProcessor`] records every processed `provider_event_id` and drops
//!    repeats, so handlers can be written as if delivery were exactly-once.
//! 3. **Answer fast.** Handlers should be short; anything slow belongs on a
//!    queue. [`WebhookOutcome`] tells the caller which HTTP status to return so
//!    that transient failures get retried and permanent ones do not.

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::sync::Arc;
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};
use crate::gateway::{GatewayEvent, GatewayId, PaymentGateway};
use crate::secret::SecretString;
use crate::storage::ProcessedEventStore;

/// Case-insensitive HTTP headers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Headers(BTreeMap<String, String>);

impl Headers {
    /// An empty header set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a header, lower-casing the name.
    pub fn insert(&mut self, name: impl AsRef<str>, value: impl Into<String>) -> &mut Self {
        self.0.insert(name.as_ref().to_ascii_lowercase(), value.into());
        self
    }

    /// Builder form of [`Self::insert`].
    pub fn with(mut self, name: impl AsRef<str>, value: impl Into<String>) -> Self {
        self.insert(name, value);
        self
    }

    /// Look up a header, ignoring case.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(&name.to_ascii_lowercase()).map(String::as_str)
    }

    /// Look up a header or fail with a precise error.
    pub fn require(&self, name: &str) -> Result<&str> {
        self.get(name)
            .ok_or_else(|| Error::WebhookVerification(format!("missing '{name}' header")))
    }

    /// Iterate over all headers.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &String)> {
        self.0.iter()
    }
}

impl<K: AsRef<str>, V: Into<String>> FromIterator<(K, V)> for Headers {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        let mut headers = Headers::new();
        for (name, value) in iter {
            headers.insert(name, value);
        }
        headers
    }
}

/// Compute the hex-encoded HMAC-SHA256 of `payload` under `secret`.
pub fn hmac_sha256_hex(secret: &SecretString, payload: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(secret.expose().as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(payload);
    hex::encode(mac.finalize().into_bytes())
}

/// Verify a hex-encoded HMAC-SHA256 signature in constant time.
///
/// The comparison is constant time so that an attacker cannot recover a valid
/// signature byte by byte by timing the endpoint.
pub fn verify_hmac_sha256(
    secret: &SecretString,
    payload: &[u8],
    expected_hex: &str,
) -> Result<()> {
    let computed = hmac_sha256_hex(secret, payload);
    let expected = expected_hex.trim().to_ascii_lowercase();
    if computed.as_bytes().ct_eq(expected.as_bytes()).into() {
        Ok(())
    } else {
        Err(Error::WebhookVerification("signature does not match".to_owned()))
    }
}

/// Reject webhooks whose timestamp is too far from now, to stop replay attacks.
pub fn verify_timestamp_freshness(
    timestamp_seconds: i64,
    now: chrono::DateTime<chrono::Utc>,
    tolerance_seconds: i64,
) -> Result<()> {
    let delta = (now.timestamp() - timestamp_seconds).abs();
    if delta > tolerance_seconds {
        return Err(Error::WebhookVerification(format!(
            "timestamp is {delta}s away from now, tolerance is {tolerance_seconds}s"
        )));
    }
    Ok(())
}

/// What the caller should do with the HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebhookOutcome {
    /// Handled. Return `200`.
    Processed,
    /// Already handled earlier. Return `200` — retrying will not help.
    Duplicate,
    /// Understood but irrelevant to us. Return `200`.
    Ignored,
}

impl WebhookOutcome {
    /// Suggested HTTP status code.
    pub fn status_code(&self) -> u16 {
        200
    }
}

/// Business logic invoked for verified, deduplicated events.
#[async_trait]
pub trait WebhookHandler: Send + Sync {
    /// React to an event. Returning an error makes the processor report a
    /// retryable failure so the provider will deliver the event again.
    async fn handle(&self, event: &GatewayEvent) -> Result<()>;
}

/// Verifies, deduplicates and dispatches webhooks for any number of gateways.
#[derive(Clone)]
pub struct WebhookProcessor {
    gateways: Vec<Arc<dyn PaymentGateway>>,
    handlers: Vec<Arc<dyn WebhookHandler>>,
    processed: Arc<dyn ProcessedEventStore>,
}

impl WebhookProcessor {
    /// Build a processor over a deduplication store.
    pub fn new(processed: Arc<dyn ProcessedEventStore>) -> Self {
        Self { gateways: Vec::new(), handlers: Vec::new(), processed }
    }

    /// Register a gateway adapter that can parse its own webhooks.
    pub fn register_gateway(mut self, gateway: Arc<dyn PaymentGateway>) -> Self {
        self.gateways.push(gateway);
        self
    }

    /// Register a handler. Handlers run in registration order.
    pub fn register_handler(mut self, handler: Arc<dyn WebhookHandler>) -> Self {
        self.handlers.push(handler);
        self
    }

    /// Process a raw callback.
    ///
    /// `payload` must be the exact bytes received; do not deserialise and
    /// re-serialise before calling this.
    pub async fn process(
        &self,
        gateway_id: &GatewayId,
        payload: &[u8],
        headers: &Headers,
    ) -> Result<WebhookOutcome> {
        let gateway = self
            .gateways
            .iter()
            .find(|candidate| &candidate.id() == gateway_id)
            .ok_or_else(|| {
                Error::configuration(format!("no gateway registered as '{gateway_id}'"))
            })?;

        // Verification happens inside the adapter, against the raw bytes.
        let event = gateway.parse_webhook(payload, headers).await?;

        let dedupe_key = format!("{gateway_id}:{}", event.provider_event_id);
        if !self.processed.mark_processed(&dedupe_key).await? {
            tracing::debug!(event = %event.provider_event_id, "duplicate webhook ignored");
            return Ok(WebhookOutcome::Duplicate);
        }

        if self.handlers.is_empty() {
            return Ok(WebhookOutcome::Ignored);
        }

        for handler in &self.handlers {
            if let Err(error) = handler.handle(&event).await {
                // Let the provider retry: un-mark so the retry is not dropped.
                self.processed.unmark(&dedupe_key).await?;
                return Err(error);
            }
        }
        Ok(WebhookOutcome::Processed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::GatewayEventKind;
    use crate::storage::memory::InMemoryProcessedEventStore;
    use chrono::Utc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingHandler(Arc<AtomicUsize>, bool);

    #[async_trait]
    impl WebhookHandler for CountingHandler {
        async fn handle(&self, _event: &GatewayEvent) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            if self.1 { Err(Error::storage("transient")) } else { Ok(()) }
        }
    }

    struct FakeGateway;

    #[async_trait]
    impl PaymentGateway for FakeGateway {
        fn id(&self) -> GatewayId {
            GatewayId::from_static("fake")
        }
        fn capabilities(&self) -> crate::gateway::Capabilities {
            crate::gateway::Capabilities::full()
        }
        async fn authorize(
            &self,
            _request: &crate::gateway::AuthorizeRequest,
        ) -> Result<crate::gateway::AuthorizeResponse> {
            unimplemented!()
        }
        async fn capture(
            &self,
            _request: &crate::gateway::CaptureRequest,
        ) -> Result<crate::gateway::CaptureResponse> {
            unimplemented!()
        }
        async fn cancel(
            &self,
            _request: &crate::gateway::CancelRequest,
        ) -> Result<crate::gateway::AuthorizeResponse> {
            unimplemented!()
        }
        async fn refund(
            &self,
            _request: &crate::gateway::GatewayRefundRequest,
        ) -> Result<crate::gateway::GatewayRefundResponse> {
            unimplemented!()
        }
        async fn fetch_transaction(
            &self,
            _transaction_id: &str,
        ) -> Result<crate::gateway::AuthorizeResponse> {
            unimplemented!()
        }
        async fn parse_webhook(
            &self,
            payload: &[u8],
            headers: &Headers,
        ) -> Result<GatewayEvent> {
            let secret = SecretString::new("whsec");
            verify_hmac_sha256(&secret, payload, headers.require("x-signature")?)?;
            Ok(GatewayEvent {
                provider_event_id: String::from_utf8_lossy(payload).to_string(),
                gateway: self.id(),
                kind: GatewayEventKind::PaymentCaptured,
                transaction_id: None,
                amount: None,
                occurred_at: Utc::now(),
                raw: None,
            })
        }
    }

    fn signed_headers(payload: &[u8]) -> Headers {
        Headers::new()
            .with("X-Signature", hmac_sha256_hex(&SecretString::new("whsec"), payload))
    }

    #[test]
    fn headers_are_case_insensitive() {
        let headers = Headers::new().with("Stripe-Signature", "abc");
        assert_eq!(headers.get("stripe-signature"), Some("abc"));
        assert_eq!(headers.get("STRIPE-SIGNATURE"), Some("abc"));
        assert!(headers.require("missing").is_err());
    }

    #[test]
    fn signature_verification_rejects_tampering() {
        let secret = SecretString::new("whsec");
        let payload = b"{\"id\":\"evt_1\"}";
        let signature = hmac_sha256_hex(&secret, payload);
        assert!(verify_hmac_sha256(&secret, payload, &signature).is_ok());
        assert!(verify_hmac_sha256(&secret, b"{\"id\":\"evt_2\"}", &signature).is_err());
        assert!(verify_hmac_sha256(&SecretString::new("other"), payload, &signature).is_err());
    }

    #[test]
    fn replay_window_is_enforced() {
        let now = Utc::now();
        assert!(verify_timestamp_freshness(now.timestamp(), now, 300).is_ok());
        assert!(verify_timestamp_freshness(now.timestamp() - 3_600, now, 300).is_err());
    }

    #[tokio::test]
    async fn duplicates_are_dropped_and_failures_stay_retryable() {
        let counter = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(InMemoryProcessedEventStore::new());
        let processor = WebhookProcessor::new(store.clone())
            .register_gateway(Arc::new(FakeGateway))
            .register_handler(Arc::new(CountingHandler(counter.clone(), false)));

        let payload = b"evt_1";
        let headers = signed_headers(payload);
        let id = GatewayId::from_static("fake");

        assert_eq!(
            processor.process(&id, payload, &headers).await.unwrap(),
            WebhookOutcome::Processed
        );
        assert_eq!(
            processor.process(&id, payload, &headers).await.unwrap(),
            WebhookOutcome::Duplicate
        );
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        // An unsigned request never reaches the handler.
        assert!(processor.process(&id, payload, &Headers::new()).await.is_err());

        // A failing handler releases the dedupe key so the retry is processed.
        let failing = WebhookProcessor::new(store.clone())
            .register_gateway(Arc::new(FakeGateway))
            .register_handler(Arc::new(CountingHandler(counter.clone(), true)));
        let payload2 = b"evt_2";
        let headers2 = signed_headers(payload2);
        assert!(failing.process(&id, payload2, &headers2).await.is_err());
        assert!(failing.process(&id, payload2, &headers2).await.is_err());
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn unknown_gateway_is_a_configuration_error() {
        let processor = WebhookProcessor::new(Arc::new(InMemoryProcessedEventStore::new()));
        let error =
            processor.process(&GatewayId::from_static("nope"), b"{}", &Headers::new()).await.unwrap_err();
        assert!(matches!(error, Error::Configuration(_)));
    }
}
