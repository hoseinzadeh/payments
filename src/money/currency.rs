//! ISO-4217 currency codes with their minor-unit exponents.

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// `(alphabetic code, minor unit exponent)` for the currencies we know about.
///
/// Unknown codes are still accepted through [`Currency::new`] so that the crate
/// never blocks a valid gateway currency, but they default to two decimals
/// unless an exponent is supplied explicitly.
const KNOWN: &[(&str, u8)] = &[
    ("AED", 2),
    ("AUD", 2),
    ("BHD", 3),
    ("BRL", 2),
    ("CAD", 2),
    ("CHF", 2),
    ("CLP", 0),
    ("CNY", 2),
    ("COP", 2),
    ("CZK", 2),
    ("DKK", 2),
    ("EUR", 2),
    ("GBP", 2),
    ("HKD", 2),
    ("HUF", 2),
    ("IDR", 2),
    ("ILS", 2),
    ("INR", 2),
    ("ISK", 0),
    ("JOD", 3),
    ("JPY", 0),
    ("KRW", 0),
    ("KWD", 3),
    ("MXN", 2),
    ("MYR", 2),
    ("NOK", 2),
    ("NZD", 2),
    ("OMR", 3),
    ("PHP", 2),
    ("PLN", 2),
    ("RON", 2),
    ("SAR", 2),
    ("SEK", 2),
    ("SGD", 2),
    ("THB", 2),
    ("TND", 3),
    ("TRY", 2),
    ("TWD", 2),
    ("USD", 2),
    ("VND", 0),
    ("ZAR", 2),
];

/// An ISO-4217 currency: a three-letter code plus the number of decimal places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Currency {
    code: [u8; 3],
    exponent: u8,
}

macro_rules! currency_consts {
    ($($name:ident => $code:expr, $exp:expr;)*) => {
        $(
            #[doc = concat!("The ", $code, " currency.")]
            pub const $name: Currency = Currency {
                code: [$code.as_bytes()[0], $code.as_bytes()[1], $code.as_bytes()[2]],
                exponent: $exp,
            };
        )*
    };
}

impl Currency {
    currency_consts! {
        USD => "USD", 2;
        EUR => "EUR", 2;
        GBP => "GBP", 2;
        JPY => "JPY", 0;
        CAD => "CAD", 2;
        AUD => "AUD", 2;
        CHF => "CHF", 2;
        SEK => "SEK", 2;
        NOK => "NOK", 2;
        DKK => "DKK", 2;
    }

    /// Look up a currency by its alphabetic code, using the built-in exponent table.
    pub fn from_code(code: &str) -> Result<Self> {
        let upper = normalise(code)?;
        let as_str = std::str::from_utf8(&upper).expect("ascii");
        let exponent = KNOWN
            .iter()
            .find(|(known, _)| *known == as_str)
            .map(|(_, exp)| *exp)
            .ok_or_else(|| Error::UnknownCurrency(as_str.to_owned()))?;
        Ok(Self { code: upper, exponent })
    }

    /// Construct a currency with an explicit exponent, for codes outside the table.
    pub fn new(code: &str, exponent: u8) -> Result<Self> {
        if exponent > 4 {
            return Err(Error::money("currency exponent must be <= 4"));
        }
        Ok(Self { code: normalise(code)?, exponent })
    }

    /// The three-letter alphabetic code.
    pub fn code(&self) -> &str {
        std::str::from_utf8(&self.code).expect("ascii")
    }

    /// Number of decimal digits in the minor unit (2 for `USD`, 0 for `JPY`).
    pub const fn exponent(&self) -> u8 {
        self.exponent
    }

    /// Minor units in one major unit (100 for `USD`, 1 for `JPY`).
    pub const fn minor_units_per_major(&self) -> u32 {
        10u32.pow(self.exponent as u32)
    }
}

fn normalise(code: &str) -> Result<[u8; 3]> {
    let bytes = code.as_bytes();
    if bytes.len() != 3 || !bytes.iter().all(|b| b.is_ascii_alphabetic()) {
        return Err(Error::UnknownCurrency(code.to_owned()));
    }
    Ok([
        bytes[0].to_ascii_uppercase(),
        bytes[1].to_ascii_uppercase(),
        bytes[2].to_ascii_uppercase(),
    ])
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Currency {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Currency::from_code(s)
    }
}

impl Serialize for Currency {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl<'de> Deserialize<'de> for Currency {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct CurrencyVisitor;

        impl Visitor<'_> for CurrencyVisitor {
            type Value = Currency;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an ISO-4217 currency code")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Currency, E> {
                Currency::from_code(v).map_err(de::Error::custom)
            }
        }

        deserializer.deserialize_str(CurrencyVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_consistent_with_consts() {
        assert!(KNOWN.windows(2).all(|w| w[0].0 < w[1].0), "KNOWN must stay sorted");
        assert_eq!(Currency::from_code("usd").unwrap(), Currency::USD);
        assert_eq!(Currency::from_code("JPY").unwrap().exponent(), 0);
        assert_eq!(Currency::USD.minor_units_per_major(), 100);
        assert_eq!(Currency::JPY.minor_units_per_major(), 1);
    }

    #[test]
    fn rejects_bad_codes() {
        assert!(Currency::from_code("US").is_err());
        assert!(Currency::from_code("XXX").is_err());
        assert!(Currency::new("XBT", 8).is_err());
        assert!(Currency::new("XBT", 4).is_ok());
    }

    #[test]
    fn serde_roundtrip() {
        let json = serde_json::to_string(&Currency::EUR).unwrap();
        assert_eq!(json, "\"EUR\"");
        let back: Currency = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Currency::EUR);
    }
}
