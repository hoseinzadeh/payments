//! Payment attempts: authorisation, confirmation, capture and cancellation.

pub mod dispute;
pub mod method;
pub mod refund;
pub mod split;
pub mod tender;

pub use dispute::{Dispute, DisputeStatus};
pub use method::{CardSummary, ChargeInitiator, PaymentMethodKind, PaymentMethodRef};
pub use refund::{RefundPlan, RefundRecord, RefundRequest, RefundScope, ShopRefund, TenderRefund};
pub use split::{FunderCharge, PlatformFeePolicy, SettlementPlan, ShopAccounts, ShopSettlement};
pub use tender::{PlannedTender, TenderKind, TenderOffer, TenderPlan};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::Metadata;
use crate::error::{Error, Result};
use crate::gateway::{AuthorizeResponse, CaptureResponse, GatewayId, NextAction, TransactionStatus};
use crate::ids::{CaptureId, OrderId, PaymentId};
use crate::money::Money;

/// Where a payment attempt is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentStatus {
    /// Created but not yet sent to the gateway.
    Created,
    /// The shopper must complete 3-D Secure or a redirect.
    RequiresAction,
    /// Funds are held, and the merchant has not yet approved capture.
    Authorized,
    /// Some of the hold has been captured; more may follow.
    PartiallyCaptured,
    /// The full amount has been captured.
    Captured,
    /// The hold was released.
    Canceled,
    /// The attempt failed or was declined.
    Failed,
    /// The hold lapsed before it was captured.
    Expired,
}

impl PaymentStatus {
    /// Whether funds can still be captured from this payment.
    pub fn is_capturable(self) -> bool {
        matches!(self, PaymentStatus::Authorized | PaymentStatus::PartiallyCaptured)
    }

    /// Whether the payment can no longer change.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            PaymentStatus::Captured
                | PaymentStatus::Canceled
                | PaymentStatus::Failed
                | PaymentStatus::Expired
        )
    }

    /// Map a gateway transaction status onto a payment status.
    pub fn from_transaction(status: TransactionStatus, fully_captured: bool) -> Self {
        match status {
            TransactionStatus::RequiresAction | TransactionStatus::Pending => {
                PaymentStatus::RequiresAction
            }
            TransactionStatus::Authorized => PaymentStatus::Authorized,
            TransactionStatus::Captured => PaymentStatus::Captured,
            TransactionStatus::PartiallyCaptured => {
                if fully_captured {
                    PaymentStatus::Captured
                } else {
                    PaymentStatus::PartiallyCaptured
                }
            }
            TransactionStatus::Canceled => PaymentStatus::Canceled,
            TransactionStatus::Failed => PaymentStatus::Failed,
        }
    }
}

/// One capture against an authorisation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    /// Our identifier.
    pub id: CaptureId,
    /// The gateway's identifier for this capture.
    pub gateway_reference: String,
    /// Amount taken.
    pub amount: Money,
    /// Whether this capture closed the authorisation.
    pub final_capture: bool,
    /// When it happened.
    pub captured_at: DateTime<Utc>,
    /// Which fulfilment triggered it, when capture follows delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulfillment_group_id: Option<crate::ids::FulfillmentGroupId>,
}

/// A single payment attempt against one gateway tender.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Payment {
    /// Identifier.
    pub id: PaymentId,
    /// The order being paid.
    pub order_id: OrderId,
    /// Gateway handling the charge.
    pub gateway: GatewayId,
    /// Gateway transaction identifier, once the authorisation exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    /// Idempotency key used for the authorisation.
    pub idempotency_key: String,
    /// Amount the payment was created for.
    pub amount: Money,
    /// Amount actually authorised.
    pub amount_authorized: Money,
    /// Amount captured so far.
    pub amount_captured: Money,
    /// Amount refunded so far.
    pub amount_refunded: Money,
    /// Lifecycle state.
    pub status: PaymentStatus,
    /// Captures made against the authorisation.
    #[serde(default)]
    pub captures: Vec<Capture>,
    /// Whether the merchant has approved capturing the held funds.
    ///
    /// Required before [`Payment::record_capture`] will accept anything, which
    /// implements the "merchant must explicitly confirm before capture" rule.
    #[serde(default)]
    pub confirmed: bool,
    /// When the hold lapses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_expires_at: Option<DateTime<Utc>>,
    /// Outstanding shopper action, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<NextAction>,
    /// Receipt label for the instrument used.
    pub instrument_label: String,
    /// Index of the tender in [`Order::tenders`](crate::order::Order::tenders)
    /// that this payment settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tender_index: Option<usize>,
    /// How this payment's amount maps onto shops, copied from the tender plan.
    #[serde(default)]
    pub shop_allocation: BTreeMap<String, Money>,
    /// Why the payment failed, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_message: Option<String>,
    /// Free-form data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Optimistic concurrency token.
    #[serde(default)]
    pub version: u64,
}

impl Payment {
    /// Create a payment for one gateway tender, before contacting the gateway.
    pub fn new(
        order_id: OrderId,
        gateway: GatewayId,
        amount: Money,
        idempotency_key: impl Into<String>,
        instrument_label: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        let zero = Money::zero(amount.currency());
        Self {
            id: PaymentId::new(),
            order_id,
            gateway,
            transaction_id: None,
            idempotency_key: idempotency_key.into(),
            amount,
            amount_authorized: zero,
            amount_captured: zero,
            amount_refunded: zero,
            status: PaymentStatus::Created,
            captures: Vec::new(),
            confirmed: false,
            authorization_expires_at: None,
            next_action: None,
            instrument_label: instrument_label.into(),
            tender_index: None,
            shop_allocation: BTreeMap::new(),
            failure_message: None,
            metadata: Metadata::new(),
            created_at: now,
            updated_at: now,
            version: 0,
        }
    }

    /// Amount still capturable from the authorisation.
    pub fn capturable_amount(&self) -> Result<Money> {
        if !self.status.is_capturable() {
            return Ok(Money::zero(self.amount.currency()));
        }
        self.amount_authorized.try_sub(self.amount_captured)
    }

    /// Amount still refundable.
    pub fn refundable_amount(&self) -> Result<Money> {
        self.amount_captured.try_sub(self.amount_refunded)
    }

    /// Whether the authorisation has lapsed at `now`.
    pub fn is_authorization_expired(&self, now: DateTime<Utc>) -> bool {
        match self.authorization_expires_at {
            Some(expiry) => self.status.is_capturable() && now >= expiry,
            None => false,
        }
    }

    /// Time left before the hold lapses.
    pub fn time_until_expiry(&self, now: DateTime<Utc>) -> Option<Duration> {
        self.authorization_expires_at.map(|expiry| expiry - now)
    }

    /// Merchant approval to capture. Idempotent.
    pub fn confirm(&mut self) -> Result<()> {
        if self.status == PaymentStatus::Created {
            return Err(Error::InvalidTransition {
                kind: "payment",
                id: self.id.to_string(),
                from: format!("{:?}", self.status),
                to: "confirmed".to_owned(),
            });
        }
        if self.status.is_terminal() && self.status != PaymentStatus::Captured {
            return Err(Error::InvalidTransition {
                kind: "payment",
                id: self.id.to_string(),
                from: format!("{:?}", self.status),
                to: "confirmed".to_owned(),
            });
        }
        self.confirmed = true;
        self.touch();
        Ok(())
    }

    /// Fold a gateway authorisation response into the aggregate.
    pub fn record_authorization(&mut self, response: &AuthorizeResponse) -> Result<()> {
        self.amount.assert_same_currency(response.amount_authorized)?;
        self.transaction_id = Some(response.transaction_id.clone());
        self.amount_authorized = response.amount_authorized;
        self.amount_captured = response.amount_captured;
        self.authorization_expires_at = response.expires_at;
        self.next_action = response.next_action.clone();

        let fully_captured =
            !response.amount_captured.is_zero() && response.amount_captured >= response.amount_authorized;
        self.status = PaymentStatus::from_transaction(response.status, fully_captured);
        if !response.amount_captured.is_zero() {
            // An automatic capture is implicitly confirmed by the merchant.
            self.confirmed = true;
            if self.captures.is_empty() {
                self.captures.push(Capture {
                    id: CaptureId::new(),
                    gateway_reference: response.transaction_id.clone(),
                    amount: response.amount_captured,
                    final_capture: fully_captured,
                    captured_at: Utc::now(),
                    fulfillment_group_id: None,
                });
            }
        }
        self.touch();
        Ok(())
    }

    /// Validate a capture before it is sent to the gateway.
    pub fn check_capture(&self, amount: Money, now: DateTime<Utc>) -> Result<()> {
        if !self.status.is_capturable() {
            return Err(Error::InvalidTransition {
                kind: "payment",
                id: self.id.to_string(),
                from: format!("{:?}", self.status),
                to: "captured".to_owned(),
            });
        }
        if !self.confirmed {
            return Err(Error::validation(
                "the capture must be confirmed by the merchant before funds are taken",
            ));
        }
        if self.is_authorization_expired(now) {
            return Err(Error::validation(
                "the authorization has expired; re-authorize the payment before capturing",
            ));
        }
        if !amount.is_positive() {
            return Err(Error::validation("capture amount must be positive"));
        }
        let capturable = self.capturable_amount()?;
        if amount > capturable {
            return Err(Error::validation(format!(
                "cannot capture {amount}: only {capturable} remains authorized"
            )));
        }
        Ok(())
    }

    /// Fold a gateway capture response into the aggregate.
    pub fn record_capture(
        &mut self,
        response: &CaptureResponse,
        fulfillment_group_id: Option<crate::ids::FulfillmentGroupId>,
    ) -> Result<()> {
        self.amount.assert_same_currency(response.amount_captured)?;
        self.amount_captured = self.amount_captured.try_add(response.amount_captured)?;
        if self.amount_captured > self.amount_authorized {
            return Err(Error::internal(format!(
                "captured {} exceeds authorized {}",
                self.amount_captured, self.amount_authorized
            )));
        }
        let final_capture = self.amount_captured == self.amount_authorized;
        self.captures.push(Capture {
            id: CaptureId::new(),
            gateway_reference: response.capture_reference.clone(),
            amount: response.amount_captured,
            final_capture,
            captured_at: Utc::now(),
            fulfillment_group_id,
        });
        self.status = if final_capture {
            PaymentStatus::Captured
        } else {
            PaymentStatus::PartiallyCaptured
        };
        self.touch();
        Ok(())
    }

    /// Mark the authorisation as released.
    pub fn record_cancellation(&mut self) -> Result<()> {
        if self.amount_captured.is_positive() {
            return Err(Error::validation(
                "cannot cancel a payment that has captured funds; refund it instead",
            ));
        }
        self.status = PaymentStatus::Canceled;
        self.next_action = None;
        self.touch();
        Ok(())
    }

    /// Mark the payment as failed.
    pub fn record_failure(&mut self, message: impl Into<String>) {
        self.status = PaymentStatus::Failed;
        self.failure_message = Some(message.into());
        self.next_action = None;
        self.touch();
    }

    /// Mark a lapsed authorisation as expired.
    pub fn record_expiry(&mut self) {
        if self.status.is_capturable() {
            self.status = PaymentStatus::Expired;
            self.touch();
        }
    }

    /// Record money returned to the shopper on this payment.
    pub fn record_refund(&mut self, amount: Money) -> Result<()> {
        let refundable = self.refundable_amount()?;
        if amount > refundable {
            return Err(Error::validation(format!(
                "cannot refund {amount}: only {refundable} is refundable on this payment"
            )));
        }
        self.amount_refunded = self.amount_refunded.try_add(amount)?;
        self.touch();
        Ok(())
    }

    fn touch(&mut self) {
        self.updated_at = Utc::now();
        self.version = self.version.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn payment() -> Payment {
        Payment::new(
            OrderId::from_string("ord_1"),
            GatewayId::from_static("mock"),
            usd(10_000),
            "key-1",
            "visa •••• 4242",
        )
    }

    fn authorization(amount: i64, captured: i64, status: TransactionStatus) -> AuthorizeResponse {
        AuthorizeResponse {
            transaction_id: "txn_1".into(),
            status,
            amount_authorized: usd(amount),
            amount_captured: usd(captured),
            expires_at: Some(Utc::now() + Duration::days(7)),
            next_action: None,
            processor_reference: None,
            raw: None,
        }
    }

    fn capture(amount: i64) -> CaptureResponse {
        CaptureResponse {
            capture_reference: format!("cap_{amount}"),
            status: TransactionStatus::PartiallyCaptured,
            amount_captured: usd(amount),
            total_captured: usd(amount),
            raw: None,
        }
    }

    #[test]
    fn authorize_then_confirm_then_capture() {
        let mut payment = payment();
        payment
            .record_authorization(&authorization(10_000, 0, TransactionStatus::Authorized))
            .unwrap();
        assert_eq!(payment.status, PaymentStatus::Authorized);
        assert_eq!(payment.capturable_amount().unwrap(), usd(10_000));

        // Capture is refused until the merchant confirms.
        assert!(payment.check_capture(usd(1_000), Utc::now()).is_err());
        payment.confirm().unwrap();
        payment.check_capture(usd(4_000), Utc::now()).unwrap();

        payment.record_capture(&capture(4_000), None).unwrap();
        assert_eq!(payment.status, PaymentStatus::PartiallyCaptured);
        assert_eq!(payment.capturable_amount().unwrap(), usd(6_000));

        payment.record_capture(&capture(6_000), None).unwrap();
        assert_eq!(payment.status, PaymentStatus::Captured);
        assert!(payment.captures.last().unwrap().final_capture);
        assert_eq!(payment.capturable_amount().unwrap(), usd(0));
    }

    #[test]
    fn automatic_capture_is_implicitly_confirmed() {
        let mut payment = payment();
        payment
            .record_authorization(&authorization(10_000, 10_000, TransactionStatus::Captured))
            .unwrap();
        assert_eq!(payment.status, PaymentStatus::Captured);
        assert!(payment.confirmed);
        assert_eq!(payment.captures.len(), 1);
        assert_eq!(payment.refundable_amount().unwrap(), usd(10_000));
    }

    #[test]
    fn cannot_capture_more_than_authorized() {
        let mut payment = payment();
        payment
            .record_authorization(&authorization(10_000, 0, TransactionStatus::Authorized))
            .unwrap();
        payment.confirm().unwrap();
        assert!(payment.check_capture(usd(10_001), Utc::now()).is_err());
        assert!(payment.check_capture(usd(0), Utc::now()).is_err());
    }

    #[test]
    fn expired_authorizations_cannot_be_captured() {
        let mut payment = payment();
        let mut response = authorization(10_000, 0, TransactionStatus::Authorized);
        response.expires_at = Some(Utc::now() - Duration::hours(1));
        payment.record_authorization(&response).unwrap();
        payment.confirm().unwrap();

        assert!(payment.is_authorization_expired(Utc::now()));
        assert!(payment.check_capture(usd(100), Utc::now()).is_err());

        payment.record_expiry();
        assert_eq!(payment.status, PaymentStatus::Expired);
        assert!(payment.status.is_terminal());
    }

    #[test]
    fn captured_payments_cannot_be_voided() {
        let mut payment = payment();
        payment
            .record_authorization(&authorization(10_000, 10_000, TransactionStatus::Captured))
            .unwrap();
        assert!(payment.record_cancellation().is_err());
    }

    #[test]
    fn refunds_cannot_exceed_captures() {
        let mut payment = payment();
        payment
            .record_authorization(&authorization(10_000, 10_000, TransactionStatus::Captured))
            .unwrap();
        payment.record_refund(usd(4_000)).unwrap();
        assert_eq!(payment.refundable_amount().unwrap(), usd(6_000));
        assert!(payment.record_refund(usd(6_001)).is_err());
        payment.record_refund(usd(6_000)).unwrap();
        assert_eq!(payment.refundable_amount().unwrap(), usd(0));
    }

    #[test]
    fn confirming_an_uncontacted_payment_is_rejected() {
        let mut payment = payment();
        assert!(payment.confirm().is_err());
    }
}
