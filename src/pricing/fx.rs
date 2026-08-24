//! Currency conversion.
//!
//! Conversion is a pluggable trait so that rates can come from your treasury
//! system, an FX provider or the payment gateway itself. Rates are quoted as
//! integer numerator/denominator pairs rather than floats so that a quote can
//! be persisted, replayed and audited to the cent.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::error::{Error, Result};
use crate::money::{Currency, Money, Rounding};

/// An exact exchange rate: `to = from * numerator / denominator`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeRate {
    /// Source currency.
    pub from: Currency,
    /// Target currency.
    pub to: Currency,
    /// Rate numerator.
    pub numerator: i64,
    /// Rate denominator.
    pub denominator: i64,
    /// When the rate was quoted, for audit and staleness checks.
    pub quoted_at: DateTime<Utc>,
    /// Markup applied over the mid-market rate, in basis points.
    #[serde(default)]
    pub markup_basis_points: i64,
}

impl ExchangeRate {
    /// Build a rate from an exact ratio.
    pub fn new(from: Currency, to: Currency, numerator: i64, denominator: i64) -> Result<Self> {
        if denominator <= 0 || numerator <= 0 {
            return Err(Error::validation("exchange rate must be positive"));
        }
        Ok(Self {
            from,
            to,
            numerator,
            denominator,
            quoted_at: Utc::now(),
            markup_basis_points: 0,
        })
    }

    /// Build a rate from a decimal string such as `"1.0873"`.
    pub fn from_decimal(from: Currency, to: Currency, rate: &str) -> Result<Self> {
        let (whole, frac) = match rate.split_once('.') {
            Some((w, f)) => (w, f),
            None => (rate, ""),
        };
        if frac.len() > 12 {
            return Err(Error::validation("exchange rate has too many digits"));
        }
        let digits = format!("{whole}{frac}");
        let numerator: i64 =
            digits.parse().map_err(|_| Error::validation(format!("invalid rate '{rate}'")))?;
        let denominator = 10i64
            .checked_pow(frac.len() as u32)
            .ok_or_else(|| Error::validation("rate scale overflow"))?;
        ExchangeRate::new(from, to, numerator, denominator)
    }

    /// Builder: add a markup (spread) in basis points.
    pub fn with_markup(mut self, basis_points: i64) -> Self {
        self.markup_basis_points = basis_points;
        self
    }

    /// Convert `amount`, adjusting for the currencies' differing exponents.
    pub fn convert(&self, amount: Money, rounding: Rounding) -> Result<Money> {
        if amount.currency() != self.from {
            return Err(Error::CurrencyMismatch { expected: self.from, actual: amount.currency() });
        }
        // Scale between minor-unit exponents, e.g. USD (2) -> JPY (0).
        let from_scale = i64::from(self.from.minor_units_per_major() as i32);
        let to_scale = i64::from(self.to.minor_units_per_major() as i32);

        let numerator = self
            .numerator
            .checked_mul(to_scale)
            .ok_or_else(|| Error::money("rate scaling overflow"))?;
        let denominator = self
            .denominator
            .checked_mul(from_scale)
            .ok_or_else(|| Error::money("rate scaling overflow"))?;

        let base = Money::from_minor(amount.minor(), self.to);
        let converted = base.mul_ratio(numerator, denominator, rounding)?;

        if self.markup_basis_points == 0 {
            return Ok(converted);
        }
        let markup = converted.mul_basis_points(self.markup_basis_points, rounding)?;
        converted.try_add(markup)
    }

    /// The inverse rate, useful for displaying prices in a buyer's currency.
    pub fn inverse(&self) -> Result<ExchangeRate> {
        Ok(ExchangeRate {
            from: self.to,
            to: self.from,
            numerator: self.denominator,
            denominator: self.numerator,
            quoted_at: self.quoted_at,
            markup_basis_points: self.markup_basis_points,
        })
    }
}

/// Source of exchange rates.
#[async_trait]
pub trait CurrencyConverter: Send + Sync {
    /// Fetch the rate to convert `from` into `to`.
    async fn rate(&self, from: Currency, to: Currency) -> Result<ExchangeRate>;

    /// Convert an amount, fetching the rate first. Identity conversions are free.
    async fn convert(&self, amount: Money, to: Currency) -> Result<Money> {
        if amount.currency() == to {
            return Ok(amount);
        }
        let rate = self.rate(amount.currency(), to).await?;
        rate.convert(amount, Rounding::HalfEven)
    }
}

/// A converter backed by a static rate table. Rates can be registered in one
/// direction only; the inverse is derived automatically.
#[derive(Debug, Clone, Default)]
pub struct StaticCurrencyConverter {
    rates: HashMap<(String, String), ExchangeRate>,
}

impl StaticCurrencyConverter {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a rate (and implicitly its inverse).
    pub fn insert(&mut self, rate: ExchangeRate) -> &mut Self {
        self.rates.insert((rate.from.code().to_owned(), rate.to.code().to_owned()), rate);
        self
    }

    /// Builder form of [`Self::insert`].
    pub fn with(mut self, rate: ExchangeRate) -> Self {
        self.insert(rate);
        self
    }
}

#[async_trait]
impl CurrencyConverter for StaticCurrencyConverter {
    async fn rate(&self, from: Currency, to: Currency) -> Result<ExchangeRate> {
        if from == to {
            return ExchangeRate::new(from, to, 1, 1);
        }
        if let Some(rate) = self.rates.get(&(from.code().to_owned(), to.code().to_owned())) {
            return Ok(rate.clone());
        }
        if let Some(rate) = self.rates.get(&(to.code().to_owned(), from.code().to_owned())) {
            return rate.inverse();
        }
        Err(Error::configuration(format!("no exchange rate configured for {from} -> {to}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_rates_are_exact() {
        let rate = ExchangeRate::from_decimal(Currency::USD, Currency::EUR, "0.92").unwrap();
        assert_eq!((rate.numerator, rate.denominator), (92, 100));

        let converted =
            rate.convert(Money::from_minor(10_000, Currency::USD), Rounding::HalfEven).unwrap();
        assert_eq!(converted, Money::from_minor(9_200, Currency::EUR));
    }

    #[test]
    fn conversion_accounts_for_differing_exponents() {
        // 1 USD = 150 JPY. $12.34 -> 1851 JPY (0 decimals).
        let rate = ExchangeRate::from_decimal(Currency::USD, Currency::JPY, "150").unwrap();
        let converted =
            rate.convert(Money::from_minor(1_234, Currency::USD), Rounding::HalfEven).unwrap();
        assert_eq!(converted, Money::from_minor(1_851, Currency::JPY));
    }

    #[test]
    fn markup_is_added_on_top() {
        let rate = ExchangeRate::from_decimal(Currency::USD, Currency::EUR, "1")
            .unwrap()
            .with_markup(200); // 2 %
        let converted =
            rate.convert(Money::from_minor(10_000, Currency::USD), Rounding::HalfEven).unwrap();
        assert_eq!(converted.minor(), 10_200);
    }

    #[tokio::test]
    async fn static_converter_derives_inverses() {
        let converter = StaticCurrencyConverter::new()
            .with(ExchangeRate::from_decimal(Currency::USD, Currency::EUR, "0.5").unwrap());

        let forward = converter.convert(Money::from_minor(1_000, Currency::USD), Currency::EUR).await.unwrap();
        assert_eq!(forward.minor(), 500);

        let backward = converter.convert(Money::from_minor(500, Currency::EUR), Currency::USD).await.unwrap();
        assert_eq!(backward.minor(), 1_000);

        let identity = converter
            .convert(Money::from_minor(1, Currency::USD), Currency::USD)
            .await
            .unwrap();
        assert_eq!(identity.minor(), 1);

        assert!(converter.rate(Currency::GBP, Currency::JPY).await.is_err());
    }

    #[test]
    fn rejects_invalid_rates() {
        assert!(ExchangeRate::new(Currency::USD, Currency::EUR, 1, 0).is_err());
        assert!(ExchangeRate::from_decimal(Currency::USD, Currency::EUR, "abc").is_err());
    }
}
