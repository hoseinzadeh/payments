//! A minimal HTTP abstraction for gateway adapters.
//!
//! The crate deliberately does **not** depend on an HTTP client. Applications
//! already have one (`reqwest`, `hyper`, `ureq`, a service mesh proxy, a
//! signing middleware), and forcing a second one into the dependency tree is
//! how you end up with two TLS stacks. Adapters are written against
//! [`HttpTransport`]; you supply a ~30-line implementation over whatever client
//! you already use.
//!
//! It also makes adapters trivially testable: [`MockTransport`] lets a unit
//! test assert on the exact request an adapter builds and feed back canned
//! provider responses, with no network involved.

use async_trait::async_trait;
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::secret::SecretString;

/// HTTP methods used by gateway APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
    /// `DELETE`
    Delete,
}

impl Method {
    /// The method name.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Delete => "DELETE",
        }
    }
}

/// An outbound request built by an adapter.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpRequest {
    /// HTTP method.
    pub method: Method,
    /// Absolute URL.
    pub url: String,
    /// Headers, excluding authentication (added by the transport or adapter).
    pub headers: BTreeMap<String, String>,
    /// Request body.
    pub body: Option<Vec<u8>>,
}

impl HttpRequest {
    /// A `GET` request.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
        }
    }

    /// A form-encoded `POST`.
    pub fn post_form(url: impl Into<String>, form: &FormBody) -> Self {
        let mut headers = BTreeMap::new();
        headers
            .insert("content-type".to_owned(), "application/x-www-form-urlencoded".to_owned());
        Self {
            method: Method::Post,
            url: url.into(),
            headers,
            body: Some(form.encode().into_bytes()),
        }
    }

    /// A JSON `POST`.
    pub fn post_json(url: impl Into<String>, body: &serde_json::Value) -> Result<Self> {
        let mut headers = BTreeMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());
        Ok(Self {
            method: Method::Post,
            url: url.into(),
            headers,
            body: Some(serde_json::to_vec(body).map_err(|error| {
                Error::internal(format!("failed to serialize request body: {error}"))
            })?),
        })
    }

    /// Builder: add a header.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into().to_ascii_lowercase(), value.into());
        self
    }

    /// Builder: add bearer authentication.
    ///
    /// The secret is only exposed here, at the moment the request is built.
    pub fn with_bearer_auth(self, secret: &SecretString) -> Self {
        self.with_header("authorization", format!("Bearer {}", secret.expose()))
    }

    /// The body as a UTF-8 string, for debugging and tests.
    pub fn body_string(&self) -> String {
        self.body
            .as_ref()
            .map(|body| String::from_utf8_lossy(body).to_string())
            .unwrap_or_default()
    }
}

/// A response from the provider.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers.
    pub headers: BTreeMap<String, String>,
    /// Raw body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// A JSON response with a status.
    pub fn json(status: u16, body: serde_json::Value) -> Self {
        Self {
            status,
            headers: BTreeMap::new(),
            body: body.to_string().into_bytes(),
        }
    }

    /// Whether the status is 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// Parse the body as JSON.
    pub fn json_body(&self) -> Result<serde_json::Value> {
        serde_json::from_slice(&self.body).map_err(|error| {
            Error::Gateway {
                gateway: "unknown".to_owned(),
                provider_code: None,
                message: format!("provider returned a non-JSON body: {error}"),
                retryable: false,
            }
        })
    }

    /// Whether the status suggests the request can be retried.
    pub fn is_retryable(&self) -> bool {
        self.status == 429 || (500..600).contains(&self.status)
    }
}

/// Something that can perform HTTP requests.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Execute a request.
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse>;
}

/// A `application/x-www-form-urlencoded` body builder with support for the
/// bracketed nesting Stripe-style APIs use (`transfer_data[destination]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FormBody {
    fields: Vec<(String, String)>,
}

impl FormBody {
    /// An empty body.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a field.
    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
        self.fields.push((key.into(), value.into()));
        self
    }

    /// Add a field only when the value is `Some`.
    pub fn set_opt(
        &mut self,
        key: impl Into<String>,
        value: Option<impl Into<String>>,
    ) -> &mut Self {
        if let Some(value) = value {
            self.set(key, value);
        }
        self
    }

    /// Builder form of [`Self::set`].
    pub fn with(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.set(key, value);
        self
    }

    /// Percent-encode the fields.
    pub fn encode(&self) -> String {
        self.fields
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Whether any field has been set.
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

/// Percent-encode a string for `application/x-www-form-urlencoded`.
///
/// Implemented here rather than pulled in as a dependency: the rule set is
/// three lines and the encoding is security-relevant enough to want in view.
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            b' ' => out.push('+'),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// A transport that returns canned responses and records what it was asked for.
///
/// Only compiled for tests and examples of downstream crates.
#[derive(Debug, Default)]
pub struct MockTransport {
    responses: std::sync::Mutex<Vec<Result<HttpResponse, String>>>,
    requests: std::sync::Mutex<Vec<HttpRequest>>,
}

impl MockTransport {
    /// A transport with no queued responses.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a response. Responses are returned in FIFO order.
    pub fn push_response(&self, response: HttpResponse) {
        self.responses.lock().expect("mock transport lock").push(Ok(response));
    }

    /// Queue a transport-level failure.
    pub fn push_error(&self, message: impl Into<String>) {
        self.responses.lock().expect("mock transport lock").push(Err(message.into()));
    }

    /// Every request the adapter made, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("mock transport lock").clone()
    }

    /// The most recent request.
    pub fn last_request(&self) -> Option<HttpRequest> {
        self.requests.lock().expect("mock transport lock").last().cloned()
    }
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse> {
        self.requests.lock().expect("mock transport lock").push(request);
        let mut responses = self.responses.lock().expect("mock transport lock");
        if responses.is_empty() {
            return Err(Error::internal("MockTransport has no queued response"));
        }
        match responses.remove(0) {
            Ok(response) => Ok(response),
            Err(message) => Err(Error::Gateway {
                gateway: "mock".to_owned(),
                provider_code: None,
                message,
                retryable: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn form_encoding_handles_nesting_and_special_characters() {
        let body = FormBody::new()
            .with("amount", "1099")
            .with("metadata[order_id]", "ord_1")
            .with("description", "Café & co");
        let encoded = body.encode();
        assert!(encoded.contains("metadata%5Border_id%5D=ord_1"));
        assert!(encoded.contains("description=Caf%C3%A9+%26+co"));
        assert!(encoded.starts_with("amount=1099&"));
    }

    #[test]
    fn optional_fields_are_skipped() {
        let mut body = FormBody::new();
        let missing: Option<String> = None;
        body.set_opt("a", Some("1"));
        body.set_opt("b", missing);
        assert_eq!(body.encode(), "a=1");
    }

    #[test]
    fn bearer_auth_is_only_materialised_on_the_request() {
        let secret = SecretString::new("sk_test_123");
        let request = HttpRequest::get("https://api.example.com").with_bearer_auth(&secret);
        assert_eq!(request.headers.get("authorization").unwrap(), "Bearer sk_test_123");
        // The secret itself still refuses to print.
        assert_eq!(format!("{secret}"), "***redacted***");
    }

    #[test]
    fn response_helpers() {
        let ok = HttpResponse::json(200, serde_json::json!({"id": "ch_1"}));
        assert!(ok.is_success());
        assert_eq!(ok.json_body().unwrap()["id"], "ch_1");

        let rate_limited = HttpResponse::json(429, serde_json::json!({}));
        assert!(!rate_limited.is_success());
        assert!(rate_limited.is_retryable());

        let bad_request = HttpResponse::json(400, serde_json::json!({}));
        assert!(!bad_request.is_retryable());
    }

    #[tokio::test]
    async fn mock_transport_records_requests_and_replays_responses() {
        let transport = MockTransport::new();
        transport.push_response(HttpResponse::json(200, serde_json::json!({"ok": true})));

        let response = transport.execute(HttpRequest::get("https://example.com/x")).await.unwrap();
        assert!(response.is_success());
        assert_eq!(transport.last_request().unwrap().url, "https://example.com/x");
        assert_eq!(transport.requests().len(), 1);

        // Running out of canned responses is an explicit error, not a hang.
        assert!(transport.execute(HttpRequest::get("https://example.com/y")).await.is_err());
    }
}
