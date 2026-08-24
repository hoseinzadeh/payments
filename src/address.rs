//! Postal addresses and tax jurisdictions.

use serde::{Deserialize, Serialize};
use std::fmt;

use crate::error::{Error, Result};

/// A postal address. Only the fields that matter for tax and shipping are
/// modelled; anything else belongs in `metadata` on the owning entity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Address {
    /// Recipient or business name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// First address line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line1: Option<String>,
    /// Second address line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line2: Option<String>,
    /// City / locality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub city: Option<String>,
    /// State, province or region code (e.g. `"CA"`, `"NSW"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Postal or ZIP code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    /// ISO-3166-1 alpha-2 country code, uppercase.
    pub country: CountryCode,
}

impl Address {
    /// Minimal constructor for the fields tax calculation needs.
    pub fn new(country: CountryCode) -> Self {
        Self { country, ..Default::default() }
    }

    /// Builder: set the region.
    pub fn with_region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Builder: set the postal code.
    pub fn with_postal_code(mut self, postal_code: impl Into<String>) -> Self {
        self.postal_code = Some(postal_code.into());
        self
    }

    /// Builder: set the city.
    pub fn with_city(mut self, city: impl Into<String>) -> Self {
        self.city = Some(city.into());
        self
    }

    /// Builder: set the first address line.
    pub fn with_line1(mut self, line1: impl Into<String>) -> Self {
        self.line1 = Some(line1.into());
        self
    }

    /// The tax jurisdiction implied by this address, from most to least specific.
    pub fn jurisdiction(&self) -> Jurisdiction {
        Jurisdiction {
            country: self.country,
            region: self.region.clone(),
            postal_code: self.postal_code.clone(),
        }
    }
}

/// An ISO-3166-1 alpha-2 country code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CountryCode([u8; 2]);

impl CountryCode {
    /// The United States.
    pub const US: CountryCode = CountryCode(*b"US");
    /// Germany.
    pub const DE: CountryCode = CountryCode(*b"DE");
    /// The United Kingdom.
    pub const GB: CountryCode = CountryCode(*b"GB");
    /// Japan.
    pub const JP: CountryCode = CountryCode(*b"JP");

    /// Parse and normalise a country code.
    pub fn new(code: &str) -> Result<Self> {
        let bytes = code.as_bytes();
        if bytes.len() != 2 || !bytes.iter().all(|b| b.is_ascii_alphabetic()) {
            return Err(Error::validation(format!("invalid country code '{code}'")));
        }
        Ok(Self([bytes[0].to_ascii_uppercase(), bytes[1].to_ascii_uppercase()]))
    }

    /// The code as a string slice.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).expect("ascii")
    }
}

impl Default for CountryCode {
    fn default() -> Self {
        CountryCode::US
    }
}

impl fmt::Display for CountryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for CountryCode {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        CountryCode::new(&value)
    }
}

impl From<CountryCode> for String {
    fn from(value: CountryCode) -> Self {
        value.as_str().to_owned()
    }
}

/// Where a taxable supply takes place.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Jurisdiction {
    /// Country of supply.
    pub country: CountryCode,
    /// State / province, when the country has sub-national tax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Postal code, for local district taxes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
}

impl Jurisdiction {
    /// A country-level jurisdiction.
    pub fn country(country: CountryCode) -> Self {
        Self { country, region: None, postal_code: None }
    }

    /// A country + region jurisdiction.
    pub fn region(country: CountryCode, region: impl Into<String>) -> Self {
        Self { country, region: Some(region.into()), postal_code: None }
    }

    /// How specific this jurisdiction is: higher wins when matching rules.
    pub fn specificity(&self) -> u8 {
        1 + u8::from(self.region.is_some()) + u8::from(self.postal_code.is_some())
    }

    /// Whether `self` (a rule) applies to `target` (an address).
    ///
    /// A rule matches when every field it *does* specify matches the target,
    /// case-insensitively. A rule with only a country therefore applies to
    /// every address in that country.
    pub fn matches(&self, target: &Jurisdiction) -> bool {
        if self.country != target.country {
            return false;
        }
        if let Some(region) = &self.region {
            match &target.region {
                Some(other) if other.eq_ignore_ascii_case(region) => {}
                _ => return false,
            }
        }
        if let Some(postal) = &self.postal_code {
            match &target.postal_code {
                Some(other) if other.eq_ignore_ascii_case(postal) => {}
                _ => return false,
            }
        }
        true
    }
}

impl fmt::Display for Jurisdiction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.country)?;
        if let Some(region) = &self.region {
            write!(f, "-{region}")?;
        }
        if let Some(postal) = &self.postal_code {
            write!(f, "/{postal}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_codes_normalise() {
        assert_eq!(CountryCode::new("us").unwrap(), CountryCode::US);
        assert!(CountryCode::new("USA").is_err());
    }

    #[test]
    fn jurisdiction_matching_is_hierarchical() {
        let address = Address::new(CountryCode::US).with_region("CA").with_postal_code("94107");
        let target = address.jurisdiction();

        assert!(Jurisdiction::country(CountryCode::US).matches(&target));
        assert!(Jurisdiction::region(CountryCode::US, "ca").matches(&target));
        assert!(!Jurisdiction::region(CountryCode::US, "NY").matches(&target));
        assert!(!Jurisdiction::country(CountryCode::DE).matches(&target));

        assert_eq!(Jurisdiction::country(CountryCode::US).specificity(), 1);
        assert_eq!(target.specificity(), 3);
    }
}
