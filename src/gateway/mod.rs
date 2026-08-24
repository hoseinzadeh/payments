//! The gateway abstraction.
//!
//! Everything above this module — pricing, splits, refunds, orders — is written
//! against [`PaymentGateway`] and never against a specific provider. Adapters
//! translate the normalised requests in this module into their provider's API
//! and normalise the responses (including error and decline codes) on the way
//! back.
//!
//! Providers differ in what they can do, so an adapter also advertises its
//! [`Capabilities`]. The engine checks capabilities *before* attempting an
//! operation and fails with [`Error::UnsupportedCapability`] rather than
//! discovering the limitation halfway through a payment.

pub mod http;
#[cfg(feature = "mock-gateway")]
pub mod mock;
#[cfg(feature = "paypal")]
pub mod paypal;
pub mod registry;
#[cfg(feature = "stripe")]
pub mod stripe;

pub use registry::{GatewayRegistry, RoutingContext, RoutingRule};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fmt;

use crate::Metadata;
use crate::error::{Error, Result};
use crate::ids::{AccountId, CustomerId, OrderId};
use crate::money::{Currency, Money};
use crate::payment::method::{ChargeInitiator, PaymentMethodKind, PaymentMethodRef};

/// Identifies a gateway adapter, e.g. `"stripe"`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GatewayId(Cow<'static, str>);

impl GatewayId {
    /// Build from a `&'static str` without allocating.
    pub const fn from_static(id: &'static str) -> Self {
        GatewayId(Cow::Borrowed(id))
    }

    /// Build from an owned string.
    pub fn new(id: impl Into<String>) -> Self {
        GatewayId(Cow::Owned(id.into()))
    }

    /// The identifier as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GatewayId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&'static str> for GatewayId {
    fn from(value: &'static str) -> Self {
        GatewayId::from_static(value)
    }
}

/// What a gateway adapter can do.
///
/// Optional features are opt-in: a new adapter that leaves everything at its
/// default is treated as a simple charge-only processor, which is always safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Supports authorising now and capturing later.
    pub delayed_capture: bool,
    /// Supports capturing less than the authorised amount.
    pub partial_capture: bool,
    /// Supports several captures against one authorisation.
    pub multi_capture: bool,
    /// Supports refunds.
    pub refunds: bool,
    /// Supports refunding part of a charge.
    pub partial_refunds: bool,
    /// Supports paying out to connected sub-accounts (marketplaces).
    pub connected_accounts: bool,
    /// Supports taking a platform fee out of a charge.
    pub application_fees: bool,
    /// Supports vaulting instruments for later off-session use.
    pub stored_credentials: bool,
    /// Emits verifiable webhooks.
    pub webhooks: bool,
    /// Reports disputes/chargebacks.
    pub disputes: bool,
    /// Supports 3-D Secure / SCA step-up.
    pub three_d_secure: bool,
    /// How long an authorisation stays valid.
    pub authorization_validity: Duration,
    /// Currencies the adapter accepts; empty means "any".
    pub currencies: BTreeSet<String>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            delayed_capture: false,
            partial_capture: false,
            multi_capture: false,
            refunds: true,
            partial_refunds: false,
            connected_accounts: false,
            application_fees: false,
            stored_credentials: false,
            webhooks: false,
            disputes: false,
            three_d_secure: false,
            authorization_validity: Duration::days(7),
            currencies: BTreeSet::new(),
        }
    }
}

impl Capabilities {
    /// A capability set with everything enabled, for full-featured adapters.
    pub fn full() -> Self {
        Self {
            delayed_capture: true,
            partial_capture: true,
            multi_capture: true,
            refunds: true,
            partial_refunds: true,
            connected_accounts: true,
            application_fees: true,
            stored_credentials: true,
            webhooks: true,
            disputes: true,
            three_d_secure: true,
            authorization_validity: Duration::days(7),
            currencies: BTreeSet::new(),
        }
    }

    /// Whether the adapter handles `currency`.
    pub fn supports_currency(&self, currency: Currency) -> bool {
        self.currencies.is_empty() || self.currencies.contains(currency.code())
    }

    /// Assert a named capability, producing a precise error when it is missing.
    pub fn require(&self, gateway: &GatewayId, capability: &'static str) -> Result<()> {
        let available = match capability {
            "delayed_capture" => self.delayed_capture,
            "partial_capture" => self.partial_capture,
            "multi_capture" => self.multi_capture,
            "refunds" => self.refunds,
            "partial_refunds" => self.partial_refunds,
            "connected_accounts" => self.connected_accounts,
            "application_fees" => self.application_fees,
            "stored_credentials" => self.stored_credentials,
            "webhooks" => self.webhooks,
            "disputes" => self.disputes,
            "three_d_secure" => self.three_d_secure,
            other => {
                return Err(Error::configuration(format!("unknown capability '{other}'")));
            }
        };
        if available {
            Ok(())
        } else {
            Err(Error::UnsupportedCapability { gateway: gateway.to_string(), capability })
        }
    }
}

/// Whether funds should be captured immediately or held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CaptureMode {
    /// Authorise and capture in one call.
    #[default]
    Automatic,
    /// Authorise only; capture later (delivery, fulfilment, confirmation).
    Manual,
}

/// A reference to the instrument to charge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstrumentRef {
    /// A single-use token produced by the gateway's client SDK.
    SingleUseToken {
        /// The token.
        token: String,
    },
    /// A vaulted instrument belonging to a customer ("card on file").
    Vaulted {
        /// The gateway's instrument token.
        token: String,
        /// The gateway's customer identifier.
        customer_token: String,
    },
}

impl InstrumentRef {
    /// Build a reference from a stored payment method.
    pub fn from_stored(method: &PaymentMethodRef) -> Result<Self> {
        match &method.gateway_customer_id {
            Some(customer_token) => Ok(InstrumentRef::Vaulted {
                token: method.gateway_token.clone(),
                customer_token: customer_token.clone(),
            }),
            None => Ok(InstrumentRef::SingleUseToken { token: method.gateway_token.clone() }),
        }
    }

    /// The instrument token.
    pub fn token(&self) -> &str {
        match self {
            InstrumentRef::SingleUseToken { token } => token,
            InstrumentRef::Vaulted { token, .. } => token,
        }
    }
}

/// Instruction to route part of a charge to a connected account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransferInstruction {
    /// Destination connected account.
    pub destination: AccountId,
    /// Amount to route.
    pub amount: Money,
    /// Appears on the recipient's statement / reporting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Groups related transfers for reconciliation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transfer_group: Option<String>,
}

/// Ask the gateway to place a hold on (or charge) an instrument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizeRequest {
    /// Key that makes retries safe. Required.
    pub idempotency_key: String,
    /// Amount to authorise.
    pub amount: Money,
    /// Instrument to charge.
    pub instrument: InstrumentRef,
    /// Our customer, for reporting and risk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<CustomerId>,
    /// The order being paid for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_id: Option<OrderId>,
    /// Immediate capture or manual capture.
    #[serde(default)]
    pub capture_mode: CaptureMode,
    /// Who initiated the charge; drives stored-credential flags.
    #[serde(default)]
    pub initiator: ChargeInitiator,
    /// Splits to connected accounts, if the gateway supports them.
    #[serde(default)]
    pub transfers: Vec<TransferInstruction>,
    /// Platform fee retained from the charge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_fee: Option<Money>,
    /// Shows on the shopper's statement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_descriptor: Option<String>,
    /// Human description for the gateway dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// URL the shopper returns to after a 3-D Secure challenge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub return_url: Option<String>,
    /// Passed through to the gateway.
    #[serde(default)]
    pub metadata: Metadata,
}

impl AuthorizeRequest {
    /// Minimal request: charge `amount` on `instrument`.
    pub fn new(idempotency_key: impl Into<String>, amount: Money, instrument: InstrumentRef) -> Self {
        Self {
            idempotency_key: idempotency_key.into(),
            amount,
            instrument,
            customer_id: None,
            order_id: None,
            capture_mode: CaptureMode::Automatic,
            initiator: ChargeInitiator::CustomerOnSession,
            transfers: Vec::new(),
            application_fee: None,
            statement_descriptor: None,
            description: None,
            return_url: None,
            metadata: Metadata::new(),
        }
    }

    /// Builder: hold the funds instead of capturing them.
    pub fn manual_capture(mut self) -> Self {
        self.capture_mode = CaptureMode::Manual;
        self
    }

    /// Builder: attach the order.
    pub fn for_order(mut self, order_id: OrderId) -> Self {
        self.order_id = Some(order_id);
        self
    }

    /// Builder: attach split instructions.
    pub fn with_transfers(mut self, transfers: Vec<TransferInstruction>) -> Self {
        self.transfers = transfers;
        self
    }

    /// Builder: retain a platform fee.
    pub fn with_application_fee(mut self, fee: Money) -> Self {
        self.application_fee = Some(fee);
        self
    }

    /// Validate internal consistency before hitting the network.
    pub fn validate(&self) -> Result<()> {
        if self.idempotency_key.trim().is_empty() {
            return Err(Error::validation("an idempotency key is required"));
        }
        if !self.amount.is_positive() {
            return Err(Error::validation("authorization amount must be positive"));
        }
        let currency = self.amount.currency();
        let mut routed = Money::zero(currency);
        for transfer in &self.transfers {
            if transfer.amount.is_negative() {
                return Err(Error::validation("transfer amounts must be non-negative"));
            }
            routed = routed.try_add(transfer.amount)?;
        }
        if let Some(fee) = self.application_fee {
            if fee.is_negative() {
                return Err(Error::validation("application fee must be non-negative"));
            }
            routed = routed.try_add(fee)?;
        }
        if routed > self.amount {
            return Err(Error::validation(format!(
                "transfers plus fees ({routed}) exceed the authorized amount ({})",
                self.amount
            )));
        }
        Ok(())
    }
}

/// State of a charge at the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionStatus {
    /// The shopper must complete an action (3-D Secure, redirect).
    RequiresAction,
    /// Funds are held but not taken.
    Authorized,
    /// Funds have been taken in full.
    Captured,
    /// Some of the authorised amount has been taken.
    PartiallyCaptured,
    /// The hold was released without taking funds.
    Canceled,
    /// The charge failed or was declined.
    Failed,
    /// Waiting on an asynchronous method (bank debit, voucher).
    Pending,
}

impl TransactionStatus {
    /// Whether money has moved.
    pub fn has_funds(self) -> bool {
        matches!(self, TransactionStatus::Captured | TransactionStatus::PartiallyCaptured)
    }

    /// Whether the transaction can still change.
    pub fn is_terminal(self) -> bool {
        matches!(self, TransactionStatus::Canceled | TransactionStatus::Failed)
    }
}

/// What the shopper must do next.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NextAction {
    /// Send the shopper to a URL (3-D Secure, PayPal approval).
    Redirect {
        /// Where to send them.
        url: String,
    },
    /// Hand a client secret to the gateway's client SDK.
    UseSdk {
        /// Opaque client-side secret.
        client_secret: String,
    },
    /// Display instructions (voucher, bank transfer reference).
    DisplayInstructions {
        /// Instruction text.
        instructions: String,
    },
}

/// The gateway's answer to an authorisation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuthorizeResponse {
    /// The gateway's transaction identifier. Store it: every later operation
    /// (capture, cancel, refund) references it.
    pub transaction_id: String,
    /// Current status.
    pub status: TransactionStatus,
    /// Amount actually authorised.
    pub amount_authorized: Money,
    /// Amount already captured (non-zero for automatic capture).
    pub amount_captured: Money,
    /// When the hold lapses, if the gateway reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
    /// Pending shopper action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<NextAction>,
    /// Network/processor detail for reconciliation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub processor_reference: Option<String>,
    /// Provider payload, retained for support and audit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// Take funds from an existing authorisation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureRequest {
    /// Idempotency key.
    pub idempotency_key: String,
    /// The authorisation to capture.
    pub transaction_id: String,
    /// Amount to take; must not exceed the remaining authorised amount.
    pub amount: Money,
    /// When `true`, release any uncaptured remainder afterwards.
    #[serde(default)]
    pub final_capture: bool,
}

/// The result of a capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureResponse {
    /// Gateway identifier for this capture.
    pub capture_reference: String,
    /// Status of the parent transaction afterwards.
    pub status: TransactionStatus,
    /// Amount taken by this capture.
    pub amount_captured: Money,
    /// Total captured against the transaction so far.
    pub total_captured: Money,
    /// Provider payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// Release an authorisation without taking funds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CancelRequest {
    /// Idempotency key.
    pub idempotency_key: String,
    /// Transaction to void.
    pub transaction_id: String,
    /// Optional reason for the gateway dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Why money is being returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RefundReason {
    /// Shopper changed their mind.
    #[default]
    RequestedByCustomer,
    /// Goods could not be supplied.
    Unavailable,
    /// Order was cancelled before fulfilment.
    OrderCanceled,
    /// Suspected fraud.
    Fraudulent,
    /// Charged in error.
    Duplicate,
    /// Anything else.
    Other,
}

/// Return funds to the shopper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayRefundRequest {
    /// Idempotency key.
    pub idempotency_key: String,
    /// The charge to refund.
    pub transaction_id: String,
    /// Amount to return.
    pub amount: Money,
    /// Reason code.
    #[serde(default)]
    pub reason: RefundReason,
    /// Pull the money back from connected accounts proportionally.
    #[serde(default)]
    pub reverse_transfers: bool,
    /// Also refund the platform fee.
    #[serde(default)]
    pub refund_application_fee: bool,
    /// Passed through to the gateway.
    #[serde(default)]
    pub metadata: Metadata,
}

/// The result of a refund.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayRefundResponse {
    /// Gateway identifier for the refund.
    pub refund_reference: String,
    /// Amount returned.
    pub amount: Money,
    /// Whether the refund has settled or is still pending.
    pub status: RefundStatus,
    /// Provider payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// Lifecycle of a refund at the gateway.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefundStatus {
    /// Submitted, not yet settled.
    Pending,
    /// Money is on its way back to the shopper.
    Succeeded,
    /// The gateway rejected the refund.
    Failed,
    /// The refund was reversed (e.g. the bank returned it).
    Canceled,
}

/// Register or look up a customer at the gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomerRequest {
    /// Our customer identifier.
    pub customer_id: CustomerId,
    /// Contact email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Passed through.
    #[serde(default)]
    pub metadata: Metadata,
}

/// Vault an instrument against a gateway customer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VaultRequest {
    /// Our customer.
    pub customer_id: CustomerId,
    /// The gateway's customer identifier.
    pub gateway_customer_id: String,
    /// A single-use token from the gateway's client SDK.
    pub setup_token: String,
    /// Explicit consent for future off-session charges.
    #[serde(default)]
    pub allow_off_session: bool,
    /// Passed through.
    #[serde(default)]
    pub metadata: Metadata,
}

/// A normalised asynchronous notification from a gateway.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GatewayEvent {
    /// Provider's event id; used for deduplication.
    pub provider_event_id: String,
    /// Which gateway sent it.
    pub gateway: GatewayId,
    /// Normalised event type.
    pub kind: GatewayEventKind,
    /// Transaction the event refers to, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    /// Amount involved, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<Money>,
    /// When the provider says it happened.
    pub occurred_at: DateTime<Utc>,
    /// The original payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<serde_json::Value>,
}

/// Normalised webhook event types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GatewayEventKind {
    /// An authorisation succeeded.
    PaymentAuthorized,
    /// Funds were captured.
    PaymentCaptured,
    /// The charge failed or was declined.
    PaymentFailed,
    /// The authorisation was voided.
    PaymentCanceled,
    /// An authorisation lapsed without capture.
    AuthorizationExpired,
    /// A refund settled.
    RefundSucceeded,
    /// A refund failed.
    RefundFailed,
    /// A chargeback was opened.
    DisputeOpened,
    /// A chargeback was resolved.
    DisputeClosed {
        /// Whether the merchant won.
        won: bool,
    },
    /// A payout to a connected account settled.
    PayoutPaid,
    /// Anything the adapter does not model.
    Unknown {
        /// The provider's own event type.
        provider_type: String,
    },
}

/// The interface every payment provider adapter implements.
///
/// Adapters must be cheap to clone or share (`Send + Sync`), and every method
/// must be idempotent with respect to the supplied idempotency key.
#[async_trait]
pub trait PaymentGateway: Send + Sync {
    /// Stable identifier for this adapter.
    fn id(&self) -> GatewayId;

    /// What this adapter supports.
    fn capabilities(&self) -> Capabilities;

    /// Create (or fetch) the provider-side customer record.
    async fn upsert_customer(&self, _request: &CustomerRequest) -> Result<String> {
        Err(Error::UnsupportedCapability {
            gateway: self.id().to_string(),
            capability: "stored_credentials",
        })
    }

    /// Vault an instrument for later use.
    async fn vault_payment_method(&self, _request: &VaultRequest) -> Result<PaymentMethodRef> {
        Err(Error::UnsupportedCapability {
            gateway: self.id().to_string(),
            capability: "stored_credentials",
        })
    }

    /// Forget a vaulted instrument.
    async fn detach_payment_method(&self, _token: &str) -> Result<()> {
        Err(Error::UnsupportedCapability {
            gateway: self.id().to_string(),
            capability: "stored_credentials",
        })
    }

    /// List the instruments vaulted for a gateway customer.
    async fn list_payment_methods(
        &self,
        _gateway_customer_id: &str,
    ) -> Result<Vec<PaymentMethodKind>> {
        Err(Error::UnsupportedCapability {
            gateway: self.id().to_string(),
            capability: "stored_credentials",
        })
    }

    /// Authorise (and optionally capture) a charge.
    async fn authorize(&self, request: &AuthorizeRequest) -> Result<AuthorizeResponse>;

    /// Capture funds from an authorisation.
    async fn capture(&self, request: &CaptureRequest) -> Result<CaptureResponse>;

    /// Release an authorisation.
    async fn cancel(&self, request: &CancelRequest) -> Result<AuthorizeResponse>;

    /// Refund a captured charge.
    async fn refund(&self, request: &GatewayRefundRequest) -> Result<GatewayRefundResponse>;

    /// Fetch the current state of a transaction, for reconciliation.
    async fn fetch_transaction(&self, transaction_id: &str) -> Result<AuthorizeResponse>;

    /// Verify a webhook's signature and normalise its payload.
    ///
    /// `payload` must be the **raw** request body: re-serialising JSON changes
    /// the bytes and breaks every signature scheme in use.
    async fn parse_webhook(
        &self,
        _payload: &[u8],
        _headers: &crate::webhook::Headers,
    ) -> Result<GatewayEvent> {
        Err(Error::UnsupportedCapability {
            gateway: self.id().to_string(),
            capability: "webhooks",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    #[test]
    fn capability_checks_name_the_missing_feature() {
        let capabilities = Capabilities::default();
        let id = GatewayId::from_static("basic");
        let error = capabilities.require(&id, "delayed_capture").unwrap_err();
        assert!(matches!(
            error,
            Error::UnsupportedCapability { capability: "delayed_capture", .. }
        ));
        assert!(capabilities.require(&id, "refunds").is_ok());
        assert!(capabilities.require(&id, "teleportation").is_err());
        assert!(Capabilities::full().require(&id, "multi_capture").is_ok());
    }

    #[test]
    fn currency_restrictions() {
        let mut capabilities = Capabilities::full();
        assert!(capabilities.supports_currency(Currency::JPY));
        capabilities.currencies.insert("USD".to_owned());
        assert!(capabilities.supports_currency(Currency::USD));
        assert!(!capabilities.supports_currency(Currency::JPY));
    }

    #[test]
    fn authorize_requests_are_validated_before_the_network_call() {
        let instrument = InstrumentRef::SingleUseToken { token: "tok".into() };
        let mut request = AuthorizeRequest::new("key-1", usd(1_000), instrument);
        assert!(request.validate().is_ok());

        request.transfers = vec![TransferInstruction {
            destination: AccountId::from_string("acct_a"),
            amount: usd(900),
            description: None,
            transfer_group: None,
        }];
        request.application_fee = Some(usd(200));
        assert!(request.validate().is_err(), "1100 routed out of a 1000 charge");

        request.application_fee = Some(usd(100));
        assert!(request.validate().is_ok());

        request.idempotency_key = "  ".into();
        assert!(request.validate().is_err());
    }

    #[test]
    fn transaction_status_helpers() {
        assert!(TransactionStatus::Captured.has_funds());
        assert!(!TransactionStatus::Authorized.has_funds());
        assert!(TransactionStatus::Failed.is_terminal());
    }
}
