//! Currency and exact minor-unit money arithmetic.
//!
//! Money is **never** represented as a float. All amounts are signed 64-bit
//! integers in the currency's minor unit (cents for `USD`, yen for `JPY`,
//! fils for `KWD`). Every operation that can lose precision requires an
//! explicit [`Rounding`] mode, and every operation that distributes an amount
//! across several recipients uses [`allocate`] so that the parts always sum
//! back to the whole.

mod allocation;
mod currency;

pub use allocation::{allocate, allocate_by_weights, allocate_evenly};
pub use currency::Currency;

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};

/// Rounding strategy used whenever an exact minor-unit result is impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Rounding {
    /// Round half away from zero (`0.5 -> 1`, `-0.5 -> -1`). Common for tax.
    #[default]
    HalfUp,
    /// Round half to the nearest even value ("banker's rounding").
    HalfEven,
    /// Round toward zero.
    Down,
    /// Round away from zero.
    Up,
    /// Round toward negative infinity.
    Floor,
    /// Round toward positive infinity.
    Ceil,
}

impl Rounding {
    /// Divide `numerator` by `denominator` applying this rounding mode.
    fn divide(self, numerator: i128, denominator: i128) -> Result<i128> {
        if denominator == 0 {
            return Err(Error::money("division by zero"));
        }
        // Normalise so the denominator is positive; keeps sign logic in one place.
        let (numerator, denominator) = if denominator < 0 {
            (-numerator, -denominator)
        } else {
            (numerator, denominator)
        };

        let quotient = numerator.div_euclid(denominator);
        let remainder = numerator.rem_euclid(denominator);
        if remainder == 0 {
            return Ok(quotient);
        }

        // `quotient` is the floor; decide whether to step up by one.
        let twice = remainder * 2;
        let step_up = match self {
            Rounding::Floor => false,
            Rounding::Ceil => true,
            Rounding::Down => numerator < 0,
            Rounding::Up => numerator > 0,
            Rounding::HalfUp => match twice.cmp(&denominator) {
                Ordering::Greater => true,
                Ordering::Less => false,
                // Exactly half: away from zero.
                Ordering::Equal => numerator > 0,
            },
            Rounding::HalfEven => match twice.cmp(&denominator) {
                Ordering::Greater => true,
                Ordering::Less => false,
                Ordering::Equal => quotient.rem_euclid(2) != 0,
            },
        };

        Ok(if step_up { quotient + 1 } else { quotient })
    }
}

/// An exact monetary amount expressed in the minor units of its [`Currency`].
///
/// ```
/// use payments::money::{Currency, Money};
///
/// let price = Money::from_minor(1_999, Currency::USD); // $19.99
/// let tax = price.mul_basis_points(825, Default::default()).unwrap(); // 8.25 %
/// assert_eq!(tax.minor(), 165);
/// assert_eq!(price.try_add(tax).unwrap().to_string(), "21.64 USD");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Money {
    /// Amount in minor units (e.g. cents).
    minor: i64,
    currency: Currency,
}

impl Money {
    /// Build an amount from minor units (cents).
    pub const fn from_minor(minor: i64, currency: Currency) -> Self {
        Self { minor, currency }
    }

    /// A zero amount in `currency`.
    pub const fn zero(currency: Currency) -> Self {
        Self { minor: 0, currency }
    }

    /// Build an amount from major units, e.g. `Money::from_major(12, Currency::USD)` == `$12.00`.
    pub fn from_major(major: i64, currency: Currency) -> Result<Self> {
        let factor = currency.minor_units_per_major() as i64;
        major
            .checked_mul(factor)
            .map(|minor| Self { minor, currency })
            .ok_or_else(|| Error::money("overflow converting major units"))
    }

    /// Parse a decimal string such as `"19.99"` in the given currency.
    ///
    /// Rejects values with more fractional digits than the currency supports,
    /// which prevents silent truncation of user input.
    pub fn parse_decimal(input: &str, currency: Currency) -> Result<Self> {
        let input = input.trim();
        let (sign, digits) = match input.strip_prefix('-') {
            Some(rest) => (-1i64, rest),
            None => (1i64, input.strip_prefix('+').unwrap_or(input)),
        };
        if digits.is_empty() {
            return Err(Error::money("empty amount"));
        }
        let (whole, frac) = match digits.split_once('.') {
            Some((w, f)) => (w, f),
            None => (digits, ""),
        };
        let exponent = currency.exponent() as usize;
        if frac.len() > exponent {
            return Err(Error::money(format!(
                "{currency} supports at most {exponent} fractional digits, got '{input}'"
            )));
        }
        if !whole.chars().all(|c| c.is_ascii_digit()) || !frac.chars().all(|c| c.is_ascii_digit()) {
            return Err(Error::money(format!("invalid decimal amount '{input}'")));
        }
        let whole: i64 = if whole.is_empty() {
            0
        } else {
            whole
                .parse()
                .map_err(|_| Error::money(format!("amount out of range: '{input}'")))?
        };
        let mut frac_value: i64 = if frac.is_empty() { 0 } else { frac.parse().unwrap_or(0) };
        for _ in frac.len()..exponent {
            frac_value *= 10;
        }
        let factor = currency.minor_units_per_major() as i64;
        let minor = whole
            .checked_mul(factor)
            .and_then(|v| v.checked_add(frac_value))
            .and_then(|v| v.checked_mul(sign))
            .ok_or_else(|| Error::money("amount out of range"))?;
        Ok(Self { minor, currency })
    }

    /// The raw minor-unit amount.
    pub const fn minor(self) -> i64 {
        self.minor
    }

    /// The currency of this amount.
    pub const fn currency(self) -> Currency {
        self.currency
    }

    /// `true` when the amount is exactly zero.
    pub const fn is_zero(self) -> bool {
        self.minor == 0
    }

    /// `true` when the amount is strictly greater than zero.
    pub const fn is_positive(self) -> bool {
        self.minor > 0
    }

    /// `true` when the amount is strictly less than zero.
    pub const fn is_negative(self) -> bool {
        self.minor < 0
    }

    /// Absolute value.
    pub fn abs(self) -> Self {
        Self { minor: self.minor.abs(), currency: self.currency }
    }

    /// Negated amount.
    pub fn negate(self) -> Self {
        Self { minor: -self.minor, currency: self.currency }
    }

    /// Clamp negative amounts to zero. Useful after subtracting discounts.
    pub fn clamp_non_negative(self) -> Self {
        Self { minor: self.minor.max(0), currency: self.currency }
    }

    /// Smaller of two amounts. Errors on currency mismatch.
    pub fn try_min(self, other: Self) -> Result<Self> {
        self.assert_same_currency(other)?;
        Ok(if self.minor <= other.minor { self } else { other })
    }

    /// Larger of two amounts. Errors on currency mismatch.
    pub fn try_max(self, other: Self) -> Result<Self> {
        self.assert_same_currency(other)?;
        Ok(if self.minor >= other.minor { self } else { other })
    }

    /// Checked addition; fails on currency mismatch or overflow.
    pub fn try_add(self, other: Self) -> Result<Self> {
        self.assert_same_currency(other)?;
        let minor = self
            .minor
            .checked_add(other.minor)
            .ok_or_else(|| Error::money("addition overflow"))?;
        Ok(Self { minor, currency: self.currency })
    }

    /// Checked subtraction; fails on currency mismatch or overflow.
    pub fn try_sub(self, other: Self) -> Result<Self> {
        self.assert_same_currency(other)?;
        let minor = self
            .minor
            .checked_sub(other.minor)
            .ok_or_else(|| Error::money("subtraction overflow"))?;
        Ok(Self { minor, currency: self.currency })
    }

    /// Multiply by an integer quantity.
    pub fn try_mul(self, factor: i64) -> Result<Self> {
        let minor = self
            .minor
            .checked_mul(factor)
            .ok_or_else(|| Error::money("multiplication overflow"))?;
        Ok(Self { minor, currency: self.currency })
    }

    /// Multiply by the rational `numerator / denominator` with explicit rounding.
    pub fn mul_ratio(self, numerator: i64, denominator: i64, rounding: Rounding) -> Result<Self> {
        let value = rounding.divide(self.minor as i128 * numerator as i128, denominator as i128)?;
        Self::from_i128(value, self.currency)
    }

    /// Multiply by a rate expressed in basis points (1 bp = 0.01 %).
    pub fn mul_basis_points(self, basis_points: i64, rounding: Rounding) -> Result<Self> {
        self.mul_ratio(basis_points, 10_000, rounding)
    }

    /// Extract the tax contained in a tax-inclusive amount at `basis_points`.
    ///
    /// For a gross of 120 at 20 % this yields 20, not 24.
    pub fn tax_from_inclusive(self, basis_points: i64, rounding: Rounding) -> Result<Self> {
        self.mul_ratio(basis_points, 10_000 + basis_points, rounding)
    }

    /// Sum an iterator of amounts, validating that they share `currency`.
    pub fn sum<I: IntoIterator<Item = Money>>(iter: I, currency: Currency) -> Result<Self> {
        iter.into_iter().try_fold(Money::zero(currency), |acc, m| acc.try_add(m))
    }

    /// Ratio of `self` to `other` in basis points, for reporting.
    pub fn ratio_basis_points(self, other: Self) -> Result<i64> {
        self.assert_same_currency(other)?;
        if other.minor == 0 {
            return Ok(0);
        }
        let value = Rounding::HalfUp.divide(self.minor as i128 * 10_000, other.minor as i128)?;
        i64::try_from(value).map_err(|_| Error::money("ratio out of range"))
    }

    /// Render as a plain decimal string without the currency code (`"19.99"`).
    pub fn to_decimal_string(self) -> String {
        let exponent = self.currency.exponent() as usize;
        if exponent == 0 {
            return self.minor.to_string();
        }
        let factor = self.currency.minor_units_per_major() as i64;
        let sign = if self.minor < 0 { "-" } else { "" };
        let abs = self.minor.unsigned_abs();
        let whole = abs / factor as u64;
        let frac = abs % factor as u64;
        format!("{sign}{whole}.{frac:0width$}", width = exponent)
    }

    fn from_i128(value: i128, currency: Currency) -> Result<Self> {
        let minor = i64::try_from(value).map_err(|_| Error::money("amount out of range"))?;
        Ok(Self { minor, currency })
    }

    pub(crate) fn assert_same_currency(self, other: Self) -> Result<()> {
        if self.currency == other.currency {
            Ok(())
        } else {
            Err(Error::CurrencyMismatch { expected: self.currency, actual: other.currency })
        }
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.to_decimal_string(), self.currency)
    }
}

impl PartialOrd for Money {
    /// Only defined for identical currencies; returns `None` otherwise.
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        if self.currency == other.currency { self.minor.partial_cmp(&other.minor) } else { None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_renders_decimals() {
        let m = Money::parse_decimal("19.99", Currency::USD).unwrap();
        assert_eq!(m.minor(), 1999);
        assert_eq!(m.to_string(), "19.99 USD");

        let jpy = Money::parse_decimal("1200", Currency::JPY).unwrap();
        assert_eq!(jpy.minor(), 1200);
        assert_eq!(jpy.to_decimal_string(), "1200");

        let kwd = Money::parse_decimal("1.234", Currency::from_code("KWD").unwrap()).unwrap();
        assert_eq!(kwd.minor(), 1234);

        assert!(Money::parse_decimal("1.999", Currency::USD).is_err());
        assert!(Money::parse_decimal("abc", Currency::USD).is_err());
        assert_eq!(Money::parse_decimal("-0.05", Currency::USD).unwrap().minor(), -5);
        assert_eq!(Money::parse_decimal(".5", Currency::USD).unwrap().minor(), 50);
    }

    #[test]
    fn rounding_modes_behave() {
        assert_eq!(Rounding::HalfUp.divide(5, 2).unwrap(), 3);
        assert_eq!(Rounding::HalfUp.divide(-5, 2).unwrap(), -3);
        assert_eq!(Rounding::HalfEven.divide(5, 2).unwrap(), 2);
        assert_eq!(Rounding::HalfEven.divide(7, 2).unwrap(), 4);
        assert_eq!(Rounding::Down.divide(-5, 2).unwrap(), -2);
        assert_eq!(Rounding::Up.divide(5, 4).unwrap(), 2);
        assert_eq!(Rounding::Floor.divide(-5, 4).unwrap(), -2);
        assert_eq!(Rounding::Ceil.divide(-5, 4).unwrap(), -1);
    }

    #[test]
    fn inclusive_tax_extraction() {
        let gross = Money::from_minor(12_000, Currency::USD);
        let tax = gross.tax_from_inclusive(2_000, Rounding::HalfUp).unwrap();
        assert_eq!(tax.minor(), 2_000);
    }

    #[test]
    fn currency_mismatch_is_an_error() {
        let usd = Money::from_minor(100, Currency::USD);
        let eur = Money::from_minor(100, Currency::EUR);
        assert!(usd.try_add(eur).is_err());
        assert!(usd.partial_cmp(&eur).is_none());
    }
}
