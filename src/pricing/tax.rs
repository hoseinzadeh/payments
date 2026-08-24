//! Gateway-agnostic tax calculation.
//!
//! Tax is deliberately pluggable: most production systems eventually delegate
//! to Avalara, TaxJar or Stripe Tax. Implement [`TaxCalculator`] to do that; the
//! bundled [`RateTableTaxCalculator`] is a complete, well-tested implementation
//! for rate-table jurisdictions and is what the tests and examples use.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

use crate::address::Jurisdiction;
use crate::error::{Error, Result};
use crate::money::{Currency, Money, Rounding};

/// Product tax classification, e.g. `standard`, `food`, `digital`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TaxCode(Cow<'static, str>);

impl TaxCode {
    /// The default rate for ordinary goods.
    pub const STANDARD: TaxCode = TaxCode(Cow::Borrowed("standard"));

    /// Goods that are not taxed at all.
    pub const EXEMPT: TaxCode = TaxCode(Cow::Borrowed("exempt"));

    /// Create a custom tax code.
    pub fn new(code: impl Into<String>) -> Self {
        TaxCode(Cow::Owned(code.into()))
    }

    /// The conventional code for shipping charges.
    pub fn shipping() -> Self {
        TaxCode(Cow::Borrowed("shipping"))
    }

    /// The code as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this code is exempt from all taxation.
    pub fn is_exempt(&self) -> bool {
        self.0 == "exempt"
    }
}

impl Default for TaxCode {
    fn default() -> Self {
        TaxCode::STANDARD
    }
}

impl fmt::Display for TaxCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for TaxCode {
    fn from(value: &str) -> Self {
        TaxCode::new(value)
    }
}

/// Whether catalogue prices already contain tax.
///
/// EU/UK/AU retail prices are normally tax **inclusive**; US prices are
/// **exclusive** and tax is added at checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaxMode {
    /// Tax is added on top of the price.
    #[default]
    Exclusive,
    /// The price already includes tax; tax is extracted from it.
    Inclusive,
}

/// One taxable amount to be priced.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxLineRequest {
    /// Caller-defined reference echoed back in the result (usually a line id).
    pub reference: String,
    /// Product classification.
    pub tax_code: TaxCode,
    /// The amount tax is computed on, *after* taxable discounts.
    pub taxable_base: Money,
}

/// Everything a calculator needs to price a whole cart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxRequest {
    /// Currency of all amounts.
    pub currency: Currency,
    /// Inclusive or exclusive pricing.
    pub mode: TaxMode,
    /// Where the supply is consumed. Drives destination-based sales tax/VAT.
    pub destination: Jurisdiction,
    /// Where the goods ship from, for origin-based jurisdictions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Jurisdiction>,
    /// The customer's tax exemption / reverse-charge registration, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_tax_id: Option<String>,
    /// Lines to tax.
    pub lines: Vec<TaxLineRequest>,
}

/// A single named component of a tax amount (state, county, city, VAT…).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxComponent {
    /// Human-readable name shown on the invoice.
    pub name: String,
    /// Rate in basis points (1 bp = 0.01 %).
    pub rate_basis_points: i64,
    /// Amount attributable to this component.
    pub amount: Money,
}

/// Tax computed for a single line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxLineResult {
    /// Echo of [`TaxLineRequest::reference`].
    pub reference: String,
    /// Total tax for the line.
    pub tax: Money,
    /// Per-jurisdiction breakdown; sums to `tax`.
    pub components: Vec<TaxComponent>,
    /// For inclusive pricing, the price net of tax.
    pub net: Money,
}

/// The result of taxing a whole request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxQuote {
    /// One entry per requested line, in the same order.
    pub lines: Vec<TaxLineResult>,
}

impl TaxQuote {
    /// Total tax across all lines.
    pub fn total(&self, currency: Currency) -> Result<Money> {
        Money::sum(self.lines.iter().map(|line| line.tax), currency)
    }

    /// Look up the tax for a reference.
    pub fn line(&self, reference: &str) -> Option<&TaxLineResult> {
        self.lines.iter().find(|line| line.reference == reference)
    }
}

/// Pluggable tax engine.
#[async_trait]
pub trait TaxCalculator: Send + Sync {
    /// Compute tax for every line in `request`.
    async fn quote(&self, request: &TaxRequest) -> Result<TaxQuote>;
}

/// A rate applicable to a jurisdiction and (optionally) a product class.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaxRule {
    /// Invoice label, e.g. `"CA State Tax"`.
    pub name: String,
    /// Where the rule applies. More specific rules win within the same `layer`.
    pub jurisdiction: Jurisdiction,
    /// Restrict to a product class; `None` applies to every class.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tax_code: Option<TaxCode>,
    /// Rate in basis points.
    pub rate_basis_points: i64,
    /// Rules in the same layer are mutually exclusive (e.g. only one "state"
    /// rate applies); rules in different layers stack.
    pub layer: String,
    /// When `true`, this rate applies to base + previously computed tax
    /// (Canadian QST style), otherwise to the base alone.
    #[serde(default)]
    pub compound: bool,
}

impl TaxRule {
    /// A simple, non-compound rule.
    pub fn new(
        name: impl Into<String>,
        layer: impl Into<String>,
        jurisdiction: Jurisdiction,
        rate_basis_points: i64,
    ) -> Self {
        Self {
            name: name.into(),
            jurisdiction,
            tax_code: None,
            rate_basis_points,
            layer: layer.into(),
            compound: false,
        }
    }

    /// Builder: restrict the rule to one product class.
    pub fn for_tax_code(mut self, code: TaxCode) -> Self {
        self.tax_code = Some(code);
        self
    }

    /// Builder: make the rule compound on top of previously applied layers.
    pub fn compounding(mut self) -> Self {
        self.compound = true;
        self
    }

    fn applies_to(&self, destination: &Jurisdiction, code: &TaxCode) -> bool {
        if !self.jurisdiction.matches(destination) {
            return false;
        }
        match &self.tax_code {
            Some(required) => required == code,
            None => true,
        }
    }
}

/// A tax engine backed by an in-memory rate table.
///
/// Within each `layer` the most specific matching rule wins (a postal-code rule
/// beats a state rule beats a country rule), and rules that name an explicit
/// `tax_code` beat catch-all rules. Layers then stack, in the order they are
/// first seen, with compound layers applying to base + accumulated tax.
#[derive(Debug, Clone, Default)]
pub struct RateTableTaxCalculator {
    rules: Vec<TaxRule>,
    rounding: Rounding,
}

impl RateTableTaxCalculator {
    /// An empty table: everything is untaxed.
    pub fn new() -> Self {
        Self { rules: Vec::new(), rounding: Rounding::HalfUp }
    }

    /// Build from a set of rules.
    pub fn with_rules(rules: impl IntoIterator<Item = TaxRule>) -> Self {
        Self { rules: rules.into_iter().collect(), rounding: Rounding::HalfUp }
    }

    /// Add one rule.
    pub fn add_rule(&mut self, rule: TaxRule) -> &mut Self {
        self.rules.push(rule);
        self
    }

    /// Override the rounding mode (default [`Rounding::HalfUp`]).
    pub fn with_rounding(mut self, rounding: Rounding) -> Self {
        self.rounding = rounding;
        self
    }

    /// Select the winning rule per layer for a destination and product class.
    fn effective_rules(&self, destination: &Jurisdiction, code: &TaxCode) -> Vec<&TaxRule> {
        let mut chosen: Vec<&TaxRule> = Vec::new();
        for rule in self.rules.iter().filter(|rule| rule.applies_to(destination, code)) {
            match chosen.iter().position(|current| current.layer == rule.layer) {
                Some(index) => {
                    let current = chosen[index];
                    let better = (
                        rule.jurisdiction.specificity(),
                        u8::from(rule.tax_code.is_some()),
                    ) > (
                        current.jurisdiction.specificity(),
                        u8::from(current.tax_code.is_some()),
                    );
                    if better {
                        chosen[index] = rule;
                    }
                }
                None => chosen.push(rule),
            }
        }
        // Non-compound layers first so compound layers see the full accumulated tax.
        chosen.sort_by_key(|rule| u8::from(rule.compound));
        chosen
    }
}

#[async_trait]
impl TaxCalculator for RateTableTaxCalculator {
    async fn quote(&self, request: &TaxRequest) -> Result<TaxQuote> {
        let currency = request.currency;
        let mut lines = Vec::with_capacity(request.lines.len());

        for line in &request.lines {
            if line.taxable_base.currency() != currency {
                return Err(Error::CurrencyMismatch {
                    expected: currency,
                    actual: line.taxable_base.currency(),
                });
            }

            let rules = if line.tax_code.is_exempt() {
                Vec::new()
            } else {
                self.effective_rules(&request.destination, &line.tax_code)
            };

            let mut components = Vec::new();
            let mut accumulated = Money::zero(currency);

            match request.mode {
                TaxMode::Exclusive => {
                    for rule in &rules {
                        let base = if rule.compound {
                            line.taxable_base.try_add(accumulated)?
                        } else {
                            line.taxable_base
                        };
                        let amount = base.mul_basis_points(rule.rate_basis_points, self.rounding)?;
                        accumulated = accumulated.try_add(amount)?;
                        components.push(TaxComponent {
                            name: rule.name.clone(),
                            rate_basis_points: rule.rate_basis_points,
                            amount,
                        });
                    }
                    lines.push(TaxLineResult {
                        reference: line.reference.clone(),
                        tax: accumulated,
                        components,
                        net: line.taxable_base,
                    });
                }
                TaxMode::Inclusive => {
                    // Extract the total tax embedded in the gross, then split it
                    // across layers proportionally so the components still sum
                    // exactly to the extracted total.
                    let combined: i64 = rules.iter().map(|rule| rule.rate_basis_points).sum();
                    let total_tax = line.taxable_base.tax_from_inclusive(combined, self.rounding)?;
                    let weights: Vec<i64> =
                        rules.iter().map(|rule| rule.rate_basis_points).collect();
                    let shares = crate::money::allocate(total_tax, &weights)?;
                    for (rule, amount) in rules.iter().zip(shares) {
                        components.push(TaxComponent {
                            name: rule.name.clone(),
                            rate_basis_points: rule.rate_basis_points,
                            amount,
                        });
                    }
                    lines.push(TaxLineResult {
                        reference: line.reference.clone(),
                        tax: total_tax,
                        components,
                        net: line.taxable_base.try_sub(total_tax)?,
                    });
                }
            }
        }

        Ok(TaxQuote { lines })
    }
}

/// A calculator that never charges tax. Useful for tests and tax-free regions.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoTaxCalculator;

#[async_trait]
impl TaxCalculator for NoTaxCalculator {
    async fn quote(&self, request: &TaxRequest) -> Result<TaxQuote> {
        Ok(TaxQuote {
            lines: request
                .lines
                .iter()
                .map(|line| TaxLineResult {
                    reference: line.reference.clone(),
                    tax: Money::zero(request.currency),
                    components: Vec::new(),
                    net: line.taxable_base,
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::CountryCode;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn california() -> RateTableTaxCalculator {
        RateTableTaxCalculator::with_rules([
            TaxRule::new("US Federal", "federal", Jurisdiction::country(CountryCode::US), 0),
            TaxRule::new(
                "CA State Tax",
                "state",
                Jurisdiction::region(CountryCode::US, "CA"),
                725,
            ),
            TaxRule::new(
                "SF District Tax",
                "district",
                Jurisdiction {
                    country: CountryCode::US,
                    region: Some("CA".into()),
                    postal_code: Some("94107".into()),
                },
                113,
            ),
            TaxRule::new("CA Groceries", "state", Jurisdiction::region(CountryCode::US, "CA"), 0)
                .for_tax_code(TaxCode::new("food")),
        ])
    }

    fn request(mode: TaxMode, lines: Vec<TaxLineRequest>) -> TaxRequest {
        TaxRequest {
            currency: Currency::USD,
            mode,
            destination: Jurisdiction {
                country: CountryCode::US,
                region: Some("CA".into()),
                postal_code: Some("94107".into()),
            },
            origin: None,
            customer_tax_id: None,
            lines,
        }
    }

    #[tokio::test]
    async fn layers_stack_and_specific_rules_win() {
        let calculator = california();
        let quote = calculator
            .quote(&request(
                TaxMode::Exclusive,
                vec![TaxLineRequest {
                    reference: "l1".into(),
                    tax_code: TaxCode::STANDARD,
                    taxable_base: usd(10_000),
                }],
            ))
            .await
            .unwrap();

        // 7.25 % state + 1.13 % district = 838 cents.
        assert_eq!(quote.lines[0].tax, usd(838));
        assert_eq!(quote.lines[0].components.len(), 3);
        let sum = Money::sum(quote.lines[0].components.iter().map(|c| c.amount), Currency::USD)
            .unwrap();
        assert_eq!(sum, quote.lines[0].tax);
    }

    #[tokio::test]
    async fn product_class_overrides_the_generic_rate() {
        let quote = california()
            .quote(&request(
                TaxMode::Exclusive,
                vec![TaxLineRequest {
                    reference: "food".into(),
                    tax_code: TaxCode::new("food"),
                    taxable_base: usd(10_000),
                }],
            ))
            .await
            .unwrap();
        // State rate replaced by the 0 % grocery rate, district still applies.
        assert_eq!(quote.lines[0].tax, usd(113));
    }

    #[tokio::test]
    async fn exempt_lines_are_never_taxed() {
        let quote = california()
            .quote(&request(
                TaxMode::Exclusive,
                vec![TaxLineRequest {
                    reference: "e".into(),
                    tax_code: TaxCode::EXEMPT,
                    taxable_base: usd(10_000),
                }],
            ))
            .await
            .unwrap();
        assert!(quote.lines[0].tax.is_zero());
    }

    #[tokio::test]
    async fn inclusive_pricing_extracts_rather_than_adds() {
        let calculator = RateTableTaxCalculator::with_rules([TaxRule::new(
            "VAT",
            "vat",
            Jurisdiction::country(CountryCode::DE),
            1_900,
        )]);
        let quote = calculator
            .quote(&TaxRequest {
                currency: Currency::EUR,
                mode: TaxMode::Inclusive,
                destination: Jurisdiction::country(CountryCode::DE),
                origin: None,
                customer_tax_id: None,
                lines: vec![TaxLineRequest {
                    reference: "l".into(),
                    tax_code: TaxCode::STANDARD,
                    taxable_base: Money::from_minor(11_900, Currency::EUR),
                }],
            })
            .await
            .unwrap();
        assert_eq!(quote.lines[0].tax, Money::from_minor(1_900, Currency::EUR));
        assert_eq!(quote.lines[0].net, Money::from_minor(10_000, Currency::EUR));
    }

    #[tokio::test]
    async fn compound_rates_apply_on_top_of_earlier_layers() {
        let calculator = RateTableTaxCalculator::with_rules([
            TaxRule::new("GST", "federal", Jurisdiction::country(CountryCode::new("CA").unwrap()), 500),
            TaxRule::new(
                "QST",
                "provincial",
                Jurisdiction::region(CountryCode::new("CA").unwrap(), "QC"),
                997,
            )
            .compounding(),
        ]);
        let quote = calculator
            .quote(&TaxRequest {
                currency: Currency::CAD,
                mode: TaxMode::Exclusive,
                destination: Jurisdiction::region(CountryCode::new("CA").unwrap(), "QC"),
                origin: None,
                customer_tax_id: None,
                lines: vec![TaxLineRequest {
                    reference: "l".into(),
                    tax_code: TaxCode::STANDARD,
                    taxable_base: Money::from_minor(10_000, Currency::CAD),
                }],
            })
            .await
            .unwrap();
        // 5 % of 100.00 = 5.00; 9.975 % of 105.00 = 10.47.
        assert_eq!(quote.lines[0].components[0].amount.minor(), 500);
        assert_eq!(quote.lines[0].components[1].amount.minor(), 1_047);
        assert_eq!(quote.lines[0].tax.minor(), 1_547);
    }

    #[tokio::test]
    async fn no_tax_calculator_is_a_valid_implementation() {
        let quote = NoTaxCalculator
            .quote(&request(
                TaxMode::Exclusive,
                vec![TaxLineRequest {
                    reference: "l".into(),
                    tax_code: TaxCode::STANDARD,
                    taxable_base: usd(999),
                }],
            ))
            .await
            .unwrap();
        assert!(quote.total(Currency::USD).unwrap().is_zero());
    }
}
