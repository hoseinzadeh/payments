//! Gateway-agnostic fraud screening.
//!
//! Processors have their own risk engines, and you should use them. This module
//! covers the part they cannot see: your own order history, your own cart
//! composition, and the policies your business wants applied *before* a card is
//! charged. It runs as a pre-authorisation gate and returns a decision plus the
//! signals that produced it, so a review queue can show a human why.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::address::CountryCode;
use crate::error::Result;
use crate::ids::{CustomerId, ShopId};
use crate::money::Money;
use crate::pricing::Quote;

/// What to do with a payment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskDecision {
    /// Proceed.
    Allow,
    /// Proceed, but flag for manual review after authorisation.
    Review,
    /// Do not charge.
    Block,
}

/// One reason contributing to a risk score.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskSignal {
    /// Machine-readable rule name.
    pub code: String,
    /// Human explanation for the review queue.
    pub description: String,
    /// Points added to the score.
    pub weight: u16,
}

/// The result of screening.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RiskAssessment {
    /// Total score, 0-100 by convention.
    pub score: u16,
    /// The decision.
    pub decision: RiskDecision,
    /// Signals that fired.
    pub signals: Vec<RiskSignal>,
}

impl RiskAssessment {
    /// A clean assessment with no signals.
    pub fn allow() -> Self {
        Self { score: 0, decision: RiskDecision::Allow, signals: Vec::new() }
    }

    /// Whether the payment may proceed.
    pub fn is_allowed(&self) -> bool {
        self.decision != RiskDecision::Block
    }

    /// A one-line summary for logs.
    pub fn summary(&self) -> String {
        let codes: Vec<&str> = self.signals.iter().map(|signal| signal.code.as_str()).collect();
        format!("{:?} (score {}): {}", self.decision, self.score, codes.join(", "))
    }
}

/// Everything the engine is allowed to look at.
#[derive(Debug, Clone, Default)]
pub struct RiskContext<'a> {
    /// The priced cart.
    pub quote: Option<&'a Quote>,
    /// The shopper, if known.
    pub customer_id: Option<CustomerId>,
    /// Country the card was issued in.
    pub card_country: Option<CountryCode>,
    /// Country the goods ship to.
    pub shipping_country: Option<CountryCode>,
    /// Country on the billing address.
    pub billing_country: Option<CountryCode>,
    /// Successful orders this customer has completed before.
    pub prior_successful_orders: u32,
    /// Payment attempts from this customer in the last hour.
    pub attempts_last_hour: u32,
    /// Distinct instruments this customer tried in the last hour.
    pub distinct_instruments_last_hour: u32,
    /// Whether the shopper's email has been verified.
    pub email_verified: bool,
    /// Client IP country, when your edge provides it.
    pub ip_country: Option<CountryCode>,
}

/// Pluggable screening engine.
#[async_trait]
pub trait FraudEngine: Send + Sync {
    /// Screen a payment.
    async fn assess(&self, context: &RiskContext<'_>) -> Result<RiskAssessment>;
}

/// Thresholds for [`RuleBasedFraudEngine`].
#[derive(Debug, Clone)]
pub struct FraudPolicy {
    /// Orders above this amount attract a signal.
    pub high_value_threshold: Option<Money>,
    /// Score at or above which the payment is queued for review.
    pub review_score: u16,
    /// Score at or above which the payment is blocked.
    pub block_score: u16,
    /// Attempts per hour above which velocity fires.
    pub max_attempts_per_hour: u32,
    /// Distinct instruments per hour above which card testing fires.
    pub max_instruments_per_hour: u32,
    /// Shops in one cart above which the order looks like a resale run.
    pub max_shops_per_order: usize,
    /// Shops that are never screened, e.g. your own first-party store.
    pub trusted_shops: Vec<ShopId>,
}

impl Default for FraudPolicy {
    fn default() -> Self {
        Self {
            high_value_threshold: None,
            review_score: 40,
            block_score: 75,
            max_attempts_per_hour: 5,
            max_instruments_per_hour: 3,
            max_shops_per_order: 6,
            trusted_shops: Vec::new(),
        }
    }
}

/// A transparent, deterministic rule engine.
///
/// Every rule is additive and named, so an assessment can always be explained
/// and replayed — which matters when a shopper asks why their order was blocked.
#[derive(Debug, Clone, Default)]
pub struct RuleBasedFraudEngine {
    policy: FraudPolicy,
}

impl RuleBasedFraudEngine {
    /// An engine with default thresholds.
    pub fn new() -> Self {
        Self::default()
    }

    /// An engine with explicit thresholds.
    pub fn with_policy(policy: FraudPolicy) -> Self {
        Self { policy }
    }

    /// The policy in use.
    pub fn policy(&self) -> &FraudPolicy {
        &self.policy
    }
}

#[async_trait]
impl FraudEngine for RuleBasedFraudEngine {
    async fn assess(&self, context: &RiskContext<'_>) -> Result<RiskAssessment> {
        let mut signals = Vec::new();

        let all_trusted = context.quote.is_some_and(|quote| {
            !quote.shop_totals.is_empty()
                && quote
                    .shop_totals
                    .iter()
                    .all(|totals| self.policy.trusted_shops.contains(&totals.shop_id))
        });
        if all_trusted {
            return Ok(RiskAssessment::allow());
        }

        if let (Some(threshold), Some(quote)) = (self.policy.high_value_threshold, context.quote)
            && quote.totals.total.currency() == threshold.currency()
            && quote.totals.total > threshold
        {
            signals.push(RiskSignal {
                code: "high_value".to_owned(),
                description: format!(
                    "order total {} exceeds the {threshold} review threshold",
                    quote.totals.total
                ),
                weight: 25,
            });
        }

        if context.attempts_last_hour > self.policy.max_attempts_per_hour {
            signals.push(RiskSignal {
                code: "velocity".to_owned(),
                description: format!(
                    "{} payment attempts in the last hour",
                    context.attempts_last_hour
                ),
                weight: 30,
            });
        }

        if context.distinct_instruments_last_hour > self.policy.max_instruments_per_hour {
            signals.push(RiskSignal {
                code: "card_testing".to_owned(),
                description: format!(
                    "{} different instruments tried in the last hour",
                    context.distinct_instruments_last_hour
                ),
                weight: 45,
            });
        }

        if let (Some(card), Some(shipping)) = (context.card_country, context.shipping_country)
            && card != shipping
        {
            signals.push(RiskSignal {
                code: "country_mismatch".to_owned(),
                description: format!("card issued in {card} but shipping to {shipping}"),
                weight: 20,
            });
        }

        if let (Some(billing), Some(ip)) = (context.billing_country, context.ip_country)
            && billing != ip
        {
            signals.push(RiskSignal {
                code: "ip_mismatch".to_owned(),
                description: format!("billing country {billing} does not match IP country {ip}"),
                weight: 15,
            });
        }

        if context.prior_successful_orders == 0 && !context.email_verified {
            signals.push(RiskSignal {
                code: "new_unverified_customer".to_owned(),
                description: "first order from an unverified email address".to_owned(),
                weight: 15,
            });
        }

        if let Some(quote) = context.quote
            && quote.shop_totals.len() > self.policy.max_shops_per_order
        {
            signals.push(RiskSignal {
                code: "many_shops".to_owned(),
                description: format!(
                    "{} shops in a single order",
                    quote.shop_totals.len()
                ),
                weight: 10,
            });
        }

        let score = signals.iter().map(|signal| u32::from(signal.weight)).sum::<u32>().min(100)
            as u16;
        let decision = if score >= self.policy.block_score {
            RiskDecision::Block
        } else if score >= self.policy.review_score {
            RiskDecision::Review
        } else {
            RiskDecision::Allow
        };

        Ok(RiskAssessment { score, decision, signals })
    }
}

/// An engine that allows everything. The default when none is configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAllFraudEngine;

#[async_trait]
impl FraudEngine for AllowAllFraudEngine {
    async fn assess(&self, _context: &RiskContext<'_>) -> Result<RiskAssessment> {
        Ok(RiskAssessment::allow())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn policy() -> FraudPolicy {
        FraudPolicy { high_value_threshold: Some(usd(100_000)), ..Default::default() }
    }

    #[tokio::test]
    async fn a_clean_order_is_allowed() {
        let engine = RuleBasedFraudEngine::with_policy(policy());
        let context = RiskContext {
            prior_successful_orders: 12,
            email_verified: true,
            card_country: Some(CountryCode::US),
            shipping_country: Some(CountryCode::US),
            ..Default::default()
        };
        let assessment = engine.assess(&context).await.unwrap();
        assert_eq!(assessment.decision, RiskDecision::Allow);
        assert!(assessment.signals.is_empty());
    }

    #[tokio::test]
    async fn card_testing_is_blocked() {
        let engine = RuleBasedFraudEngine::with_policy(policy());
        let context = RiskContext {
            attempts_last_hour: 9,
            distinct_instruments_last_hour: 8,
            ..Default::default()
        };
        let assessment = engine.assess(&context).await.unwrap();
        assert_eq!(assessment.decision, RiskDecision::Block);
        assert!(!assessment.is_allowed());
        assert!(assessment.signals.iter().any(|signal| signal.code == "card_testing"));
        assert!(assessment.summary().contains("card_testing"));
    }

    #[tokio::test]
    async fn a_mismatched_new_customer_lands_in_review() {
        let engine = RuleBasedFraudEngine::with_policy(policy());
        let context = RiskContext {
            card_country: Some(CountryCode::US),
            shipping_country: Some(CountryCode::DE),
            billing_country: Some(CountryCode::US),
            ip_country: Some(CountryCode::DE),
            prior_successful_orders: 0,
            email_verified: false,
            ..Default::default()
        };
        let assessment = engine.assess(&context).await.unwrap();
        assert_eq!(assessment.decision, RiskDecision::Review);
        assert!(assessment.is_allowed(), "review still lets the charge through");
        assert_eq!(assessment.score, 50);
    }

    #[tokio::test]
    async fn scores_are_capped_at_100() {
        let engine = RuleBasedFraudEngine::with_policy(policy());
        let context = RiskContext {
            attempts_last_hour: 99,
            distinct_instruments_last_hour: 99,
            card_country: Some(CountryCode::US),
            shipping_country: Some(CountryCode::JP),
            billing_country: Some(CountryCode::US),
            ip_country: Some(CountryCode::JP),
            prior_successful_orders: 0,
            email_verified: false,
            ..Default::default()
        };
        let assessment = engine.assess(&context).await.unwrap();
        assert_eq!(assessment.score, 100);
    }

    #[tokio::test]
    async fn the_allow_all_engine_never_blocks() {
        let assessment = AllowAllFraudEngine.assess(&RiskContext::default()).await.unwrap();
        assert_eq!(assessment.decision, RiskDecision::Allow);
    }
}
