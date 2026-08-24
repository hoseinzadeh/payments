//! Disputes and chargebacks.
//!
//! A dispute is a deadline-driven workflow: the acquirer gives you a fixed
//! window to submit evidence, and missing it loses the case by default. The
//! model therefore makes the due date and the evidence bundle first-class.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::gateway::GatewayId;
use crate::ids::{DisputeId, OrderId, PaymentId};
use crate::money::Money;

/// Why the cardholder disputed the charge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DisputeReason {
    /// The cardholder says they did not authorise the charge.
    Fraudulent,
    /// The goods or services never arrived.
    ProductNotReceived,
    /// What arrived was not what was ordered.
    ProductUnacceptable,
    /// A promised refund never appeared.
    CreditNotProcessed,
    /// The charge appears twice.
    Duplicate,
    /// A recurring charge the cardholder had cancelled.
    SubscriptionCanceled,
    /// Anything else.
    #[default]
    General,
}

/// Where a dispute stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisputeStatus {
    /// The issuer asked for information but has not taken the money yet.
    InquiryOpen,
    /// Funds have been withdrawn pending the outcome.
    NeedsResponse,
    /// Evidence has been submitted; waiting on the issuer.
    UnderReview,
    /// The merchant won; funds are returned.
    Won,
    /// The merchant lost; funds stay with the cardholder.
    Lost,
    /// The merchant chose not to contest.
    Accepted,
}

impl DisputeStatus {
    /// Whether the case is closed.
    pub fn is_terminal(self) -> bool {
        matches!(self, DisputeStatus::Won | DisputeStatus::Lost | DisputeStatus::Accepted)
    }

    /// Whether evidence can still be submitted.
    pub fn accepts_evidence(self) -> bool {
        matches!(self, DisputeStatus::InquiryOpen | DisputeStatus::NeedsResponse)
    }
}

/// Documents supporting the merchant's case.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DisputeEvidence {
    /// Proof the goods were delivered (tracking number, signature).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shipping_documentation: Option<String>,
    /// The receipt sent to the customer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
    /// Correspondence with the customer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_communication: Option<String>,
    /// The refund policy the customer accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refund_policy: Option<String>,
    /// Free-form narrative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    /// Provider-specific extra fields.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub additional: BTreeMap<String, String>,
}

impl DisputeEvidence {
    /// Whether anything at all has been supplied.
    pub fn is_empty(&self) -> bool {
        self.shipping_documentation.is_none()
            && self.receipt.is_none()
            && self.customer_communication.is_none()
            && self.refund_policy.is_none()
            && self.explanation.is_none()
            && self.additional.is_empty()
    }
}

/// A chargeback or pre-dispute inquiry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dispute {
    /// Identifier.
    pub id: DisputeId,
    /// The disputed payment.
    pub payment_id: PaymentId,
    /// The order it belongs to.
    pub order_id: OrderId,
    /// Gateway reporting the dispute.
    pub gateway: GatewayId,
    /// Provider's dispute identifier.
    pub provider_reference: String,
    /// Amount in dispute.
    pub amount: Money,
    /// Non-refundable fee charged by the acquirer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fee: Option<Money>,
    /// Why the cardholder disputed.
    #[serde(default)]
    pub reason: DisputeReason,
    /// Current state.
    pub status: DisputeStatus,
    /// Deadline for submitting evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_due_by: Option<DateTime<Utc>>,
    /// Evidence submitted so far.
    #[serde(default)]
    pub evidence: DisputeEvidence,
    /// Which shops the disputed amount came from, so losses can be passed on.
    #[serde(default)]
    pub shop_allocation: BTreeMap<String, Money>,
    /// When the dispute was opened.
    pub opened_at: DateTime<Utc>,
    /// When it closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
    /// Optimistic concurrency token.
    #[serde(default)]
    pub version: u64,
}

impl Dispute {
    /// Open a new dispute.
    pub fn open(
        payment_id: PaymentId,
        order_id: OrderId,
        gateway: GatewayId,
        provider_reference: impl Into<String>,
        amount: Money,
    ) -> Self {
        Self {
            id: DisputeId::new(),
            payment_id,
            order_id,
            gateway,
            provider_reference: provider_reference.into(),
            amount,
            fee: None,
            reason: DisputeReason::General,
            status: DisputeStatus::NeedsResponse,
            evidence_due_by: None,
            evidence: DisputeEvidence::default(),
            shop_allocation: BTreeMap::new(),
            opened_at: Utc::now(),
            closed_at: None,
            version: 0,
        }
    }

    /// Builder: set the evidence deadline.
    pub fn due_by(mut self, at: DateTime<Utc>) -> Self {
        self.evidence_due_by = Some(at);
        self
    }

    /// Whether the response window has closed.
    pub fn is_overdue(&self, now: DateTime<Utc>) -> bool {
        match self.evidence_due_by {
            Some(due) => self.status.accepts_evidence() && now >= due,
            None => false,
        }
    }

    /// Attach evidence, rejecting late or pointless submissions.
    pub fn submit_evidence(
        &mut self,
        evidence: DisputeEvidence,
        now: DateTime<Utc>,
    ) -> Result<()> {
        if !self.status.accepts_evidence() {
            return Err(Error::InvalidTransition {
                kind: "dispute",
                id: self.id.to_string(),
                from: format!("{:?}", self.status),
                to: "under_review".to_owned(),
            });
        }
        if self.is_overdue(now) {
            return Err(Error::validation("the evidence deadline for this dispute has passed"));
        }
        if evidence.is_empty() {
            return Err(Error::validation("cannot submit an empty evidence bundle"));
        }
        self.evidence = evidence;
        self.status = DisputeStatus::UnderReview;
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Give up and let the cardholder keep the money.
    pub fn accept(&mut self, now: DateTime<Utc>) -> Result<()> {
        if self.status.is_terminal() {
            return Err(Error::InvalidTransition {
                kind: "dispute",
                id: self.id.to_string(),
                from: format!("{:?}", self.status),
                to: "accepted".to_owned(),
            });
        }
        self.status = DisputeStatus::Accepted;
        self.closed_at = Some(now);
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Record the issuer's decision.
    pub fn close(&mut self, won: bool, now: DateTime<Utc>) -> Result<()> {
        if self.status.is_terminal() {
            return Err(Error::InvalidTransition {
                kind: "dispute",
                id: self.id.to_string(),
                from: format!("{:?}", self.status),
                to: if won { "won".to_owned() } else { "lost".to_owned() },
            });
        }
        self.status = if won { DisputeStatus::Won } else { DisputeStatus::Lost };
        self.closed_at = Some(now);
        self.version = self.version.saturating_add(1);
        Ok(())
    }

    /// Total cost to the business if the dispute is lost.
    pub fn total_exposure(&self) -> Result<Money> {
        match self.fee {
            Some(fee) => self.amount.try_add(fee),
            None => Ok(self.amount),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;
    use chrono::Duration;

    fn dispute() -> Dispute {
        Dispute::open(
            PaymentId::from_string("pay_1"),
            OrderId::from_string("ord_1"),
            GatewayId::from_static("mock"),
            "dp_provider_1",
            Money::from_minor(10_000, Currency::USD),
        )
    }

    fn evidence() -> DisputeEvidence {
        DisputeEvidence {
            shipping_documentation: Some("1Z999".into()),
            ..Default::default()
        }
    }

    #[test]
    fn evidence_submission_moves_to_review() {
        let now = Utc::now();
        let mut dispute = dispute().due_by(now + Duration::days(7));
        dispute.submit_evidence(evidence(), now).unwrap();
        assert_eq!(dispute.status, DisputeStatus::UnderReview);
        assert!(dispute.submit_evidence(evidence(), now).is_err(), "already submitted");
    }

    #[test]
    fn late_or_empty_evidence_is_refused() {
        let now = Utc::now();
        let mut overdue = dispute().due_by(now - Duration::days(1));
        assert!(overdue.is_overdue(now));
        assert!(overdue.submit_evidence(evidence(), now).is_err());

        let mut in_time = dispute().due_by(now + Duration::days(1));
        assert!(in_time.submit_evidence(DisputeEvidence::default(), now).is_err());
    }

    #[test]
    fn closing_is_final() {
        let now = Utc::now();
        let mut dispute = dispute();
        dispute.close(true, now).unwrap();
        assert_eq!(dispute.status, DisputeStatus::Won);
        assert!(dispute.status.is_terminal());
        assert!(dispute.close(false, now).is_err());
        assert!(dispute.accept(now).is_err());
    }

    #[test]
    fn exposure_includes_the_dispute_fee() {
        let mut dispute = dispute();
        assert_eq!(dispute.total_exposure().unwrap(), Money::from_minor(10_000, Currency::USD));
        dispute.fee = Some(Money::from_minor(1_500, Currency::USD));
        assert_eq!(dispute.total_exposure().unwrap(), Money::from_minor(11_500, Currency::USD));
    }
}
