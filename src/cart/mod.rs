//! Shopping cart modelling and mutation.
//!
//! A [`Cart`] is a mutable, versioned aggregate. All mutations go through
//! methods that keep the invariants intact (single currency, positive
//! quantities, no orphaned fulfilment groups) and bump [`Cart::version`], which
//! the repositories use for optimistic concurrency control.

pub mod fulfillment;
pub mod service;

pub use fulfillment::{
    FulfillmentGroup, FulfillmentKey, FulfillmentMethod, FulfillmentSelection, FulfillmentStatus,
};
pub use service::CartService;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::Metadata;
use crate::address::Address;
use crate::error::{Error, Result};
use crate::ids::{CartId, CustomerId, FulfillmentGroupId, LineItemId, ShopId};
use crate::money::{Currency, Money};
use crate::pricing::TaxCode;

/// A line in a shopping cart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CartItem {
    /// Stable identifier for this line.
    pub id: LineItemId,
    /// Shop that sells the item; drives split payments and fulfilment grouping.
    pub shop_id: ShopId,
    /// Merchant SKU.
    pub sku: String,
    /// Display name.
    pub name: String,
    /// Price of one unit, exclusive or inclusive of tax depending on the
    /// [`TaxMode`](crate::pricing::TaxMode) used at pricing time.
    pub unit_price: Money,
    /// Number of units. Always `>= 1`.
    pub quantity: u32,
    /// Product tax classification.
    #[serde(default)]
    pub tax_code: TaxCode,
    /// How the buyer wants this item delivered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulfillment: Option<FulfillmentSelection>,
    /// Resolved fulfilment group; set by [`Cart::regroup_fulfillment`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fulfillment_group_id: Option<FulfillmentGroupId>,
    /// Arbitrary merchant data carried through to the order.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
}

impl CartItem {
    /// Create a line item with a generated id.
    pub fn new(
        shop_id: impl Into<ShopId>,
        sku: impl Into<String>,
        name: impl Into<String>,
        unit_price: Money,
        quantity: u32,
    ) -> Result<Self> {
        if quantity == 0 {
            return Err(Error::validation("quantity must be at least 1"));
        }
        if unit_price.is_negative() {
            return Err(Error::validation("unit price cannot be negative"));
        }
        Ok(Self {
            id: LineItemId::new(),
            shop_id: shop_id.into(),
            sku: sku.into(),
            name: name.into(),
            unit_price,
            quantity,
            tax_code: TaxCode::default(),
            fulfillment: None,
            fulfillment_group_id: None,
            metadata: Metadata::new(),
        })
    }

    /// Builder: set the tax code.
    pub fn with_tax_code(mut self, tax_code: TaxCode) -> Self {
        self.tax_code = tax_code;
        self
    }

    /// Builder: set the fulfilment selection.
    pub fn with_fulfillment(mut self, selection: FulfillmentSelection) -> Self {
        self.fulfillment = Some(selection);
        self
    }

    /// Builder: attach metadata.
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    /// Builder: override the generated id (useful for deterministic tests).
    pub fn with_id(mut self, id: LineItemId) -> Self {
        self.id = id;
        self
    }

    /// `unit_price * quantity`.
    pub fn subtotal(&self) -> Result<Money> {
        self.unit_price.try_mul(i64::from(self.quantity))
    }
}

/// A versioned shopping cart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cart {
    /// Identifier.
    pub id: CartId,
    /// Owner of the cart, if the shopper is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<CustomerId>,
    /// The single currency all amounts in this cart are denominated in.
    pub currency: Currency,
    /// Line items.
    pub items: Vec<CartItem>,
    /// Shipments derived from the items' fulfilment selections.
    #[serde(default)]
    pub fulfillment_groups: Vec<FulfillmentGroup>,
    /// Promotion codes entered by the shopper. Resolved at pricing time.
    #[serde(default)]
    pub discount_codes: Vec<String>,
    /// Gift cards the shopper wants to redeem, by code.
    #[serde(default)]
    pub gift_card_codes: Vec<String>,
    /// Whether shop credit should be applied automatically at checkout.
    #[serde(default)]
    pub apply_shop_credit: bool,
    /// Where the goods go. Drives destination-based tax.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shipping_address: Option<Address>,
    /// Billing address of the payment instrument.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub billing_address: Option<Address>,
    /// Arbitrary merchant data.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: Metadata,
    /// Optimistic concurrency token; incremented by every mutation.
    #[serde(default)]
    pub version: u64,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Last mutation timestamp.
    pub updated_at: DateTime<Utc>,
}

impl Cart {
    /// Create an empty cart in `currency`.
    pub fn new(currency: Currency) -> Self {
        let now = Utc::now();
        Self {
            id: CartId::new(),
            customer_id: None,
            currency,
            items: Vec::new(),
            fulfillment_groups: Vec::new(),
            discount_codes: Vec::new(),
            gift_card_codes: Vec::new(),
            apply_shop_credit: false,
            shipping_address: None,
            billing_address: None,
            metadata: Metadata::new(),
            version: 0,
            created_at: now,
            updated_at: now,
        }
    }

    /// Create an empty cart owned by `customer`.
    pub fn for_customer(customer: CustomerId, currency: Currency) -> Self {
        let mut cart = Cart::new(currency);
        cart.customer_id = Some(customer);
        cart
    }

    /// Add an item, merging with an existing identical line when possible.
    ///
    /// Two lines merge when they share shop, SKU, unit price, tax code and
    /// fulfilment selection; otherwise a new line is appended. This matches
    /// shopper expectations ("add to cart" twice shows quantity 2) without
    /// silently merging items that must ship separately.
    pub fn add_item(&mut self, item: CartItem) -> Result<&CartItem> {
        self.assert_currency(item.unit_price)?;
        if item.quantity == 0 {
            return Err(Error::validation("quantity must be at least 1"));
        }

        let existing = self.items.iter().position(|candidate| {
            candidate.shop_id == item.shop_id
                && candidate.sku == item.sku
                && candidate.unit_price == item.unit_price
                && candidate.tax_code == item.tax_code
                && candidate.fulfillment == item.fulfillment
                && candidate.metadata == item.metadata
        });

        let index = match existing {
            Some(index) => {
                let line = &mut self.items[index];
                line.quantity = line
                    .quantity
                    .checked_add(item.quantity)
                    .ok_or_else(|| Error::validation("quantity overflow"))?;
                index
            }
            None => {
                self.items.push(item);
                self.items.len() - 1
            }
        };
        self.touch();
        Ok(&self.items[index])
    }

    /// Change the quantity of a line. A quantity of zero removes it.
    pub fn set_quantity(&mut self, line: &LineItemId, quantity: u32) -> Result<()> {
        if quantity == 0 {
            return self.remove_item(line);
        }
        let item = self
            .items
            .iter_mut()
            .find(|item| &item.id == line)
            .ok_or_else(|| Error::not_found(LineItemId::kind(), line))?;
        item.quantity = quantity;
        self.touch();
        Ok(())
    }

    /// Remove a line item and detach it from its fulfilment group.
    pub fn remove_item(&mut self, line: &LineItemId) -> Result<()> {
        let before = self.items.len();
        self.items.retain(|item| &item.id != line);
        if self.items.len() == before {
            return Err(Error::not_found(LineItemId::kind(), line));
        }
        for group in &mut self.fulfillment_groups {
            group.items.retain(|id| id != line);
        }
        self.fulfillment_groups.retain(|group| !group.items.is_empty());
        self.touch();
        Ok(())
    }

    /// Remove every line and group.
    pub fn clear(&mut self) {
        self.items.clear();
        self.fulfillment_groups.clear();
        self.touch();
    }

    /// Look up a line item.
    pub fn item(&self, line: &LineItemId) -> Result<&CartItem> {
        self.items
            .iter()
            .find(|item| &item.id == line)
            .ok_or_else(|| Error::not_found(LineItemId::kind(), line))
    }

    /// Mutable access to a line item.
    pub fn item_mut(&mut self, line: &LineItemId) -> Result<&mut CartItem> {
        let found = self.items.iter_mut().find(|item| &item.id == line);
        match found {
            Some(item) => Ok(item),
            None => Err(Error::not_found(LineItemId::kind(), line)),
        }
    }

    /// Add a promotion code (idempotent, case-insensitive).
    pub fn apply_discount_code(&mut self, code: impl Into<String>) {
        let code = code.into();
        if !self.discount_codes.iter().any(|existing| existing.eq_ignore_ascii_case(&code)) {
            self.discount_codes.push(code);
            self.touch();
        }
    }

    /// Remove a promotion code.
    pub fn remove_discount_code(&mut self, code: &str) {
        self.discount_codes.retain(|existing| !existing.eq_ignore_ascii_case(code));
        self.touch();
    }

    /// Stage a gift card for redemption at checkout (idempotent).
    pub fn add_gift_card(&mut self, code: impl Into<String>) {
        let code = code.into();
        if !self.gift_card_codes.iter().any(|existing| existing.eq_ignore_ascii_case(&code)) {
            self.gift_card_codes.push(code);
            self.touch();
        }
    }

    /// Remove a staged gift card.
    pub fn remove_gift_card(&mut self, code: &str) {
        self.gift_card_codes.retain(|existing| !existing.eq_ignore_ascii_case(code));
        self.touch();
    }

    /// Choose whether the shopper's shop credit is spent at checkout.
    ///
    /// Use this rather than assigning [`Cart::apply_shop_credit`] directly: it
    /// bumps [`Cart::version`], without which the repository would reject the
    /// save as a stale write.
    pub fn set_apply_shop_credit(&mut self, apply: bool) {
        if self.apply_shop_credit != apply {
            self.apply_shop_credit = apply;
            self.touch();
        }
    }

    /// Set the destination address.
    pub fn set_shipping_address(&mut self, address: Address) {
        self.shipping_address = Some(address);
        self.touch();
    }

    /// Set the billing address.
    pub fn set_billing_address(&mut self, address: Address) {
        self.billing_address = Some(address);
        self.touch();
    }

    /// Sum of `unit_price * quantity` over all lines, before discounts and tax.
    pub fn subtotal(&self) -> Result<Money> {
        let mut total = Money::zero(self.currency);
        for item in &self.items {
            total = total.try_add(item.subtotal()?)?;
        }
        Ok(total)
    }

    /// Total number of units in the cart.
    pub fn unit_count(&self) -> u64 {
        self.items.iter().map(|item| u64::from(item.quantity)).sum()
    }

    /// Distinct shops represented in the cart, in first-seen order.
    pub fn shops(&self) -> Vec<ShopId> {
        let mut shops = Vec::new();
        for item in &self.items {
            if !shops.contains(&item.shop_id) {
                shops.push(item.shop_id.clone());
            }
        }
        shops
    }

    /// `true` when items from more than one shop are present, i.e. the order
    /// will need a split settlement.
    pub fn is_multi_shop(&self) -> bool {
        self.shops().len() > 1
    }

    /// Rebuild [`Cart::fulfillment_groups`] from the items' selections.
    ///
    /// `rate` is asked for the shipping charge of each distinct shipment; return
    /// zero for free shipping. Groups are deterministic, so re-running this on
    /// an unchanged cart is a no-op.
    pub fn regroup_fulfillment<F>(&mut self, mut rate: F) -> Result<()>
    where
        F: FnMut(&FulfillmentKey) -> Result<Money>,
    {
        let currency = self.currency;
        let mut groups: Vec<FulfillmentGroup> = Vec::new();
        let mut assignments: Vec<(LineItemId, FulfillmentGroupId)> = Vec::new();

        for item in &self.items {
            let selection = item.fulfillment.clone().unwrap_or_default();
            let key = selection.key(&item.shop_id);
            let id = key.deterministic_id();

            if !groups.iter().any(|group| group.id == id) {
                let price = rate(&key)?;
                if price.currency() != currency {
                    return Err(Error::CurrencyMismatch {
                        expected: currency,
                        actual: price.currency(),
                    });
                }
                let mut group = FulfillmentGroup::from_key(key, price);
                group.destination = self.shipping_address.clone();
                groups.push(group);
            }
            let group = groups.iter_mut().find(|group| group.id == id).expect("just inserted");
            group.items.push(item.id.clone());
            assignments.push((item.id.clone(), id));
        }

        for (line, group_id) in assignments {
            if let Some(item) = self.items.iter_mut().find(|item| item.id == line) {
                item.fulfillment_group_id = Some(group_id);
            }
        }
        self.fulfillment_groups = groups;
        self.touch();
        Ok(())
    }

    /// Validate every cart invariant. Called before checkout.
    pub fn validate(&self) -> Result<()> {
        if self.items.is_empty() {
            return Err(Error::validation("cart is empty"));
        }
        for item in &self.items {
            if item.quantity == 0 {
                return Err(Error::validation(format!("line {} has quantity 0", item.id)));
            }
            if item.unit_price.currency() != self.currency {
                return Err(Error::CurrencyMismatch {
                    expected: self.currency,
                    actual: item.unit_price.currency(),
                });
            }
            if item.unit_price.is_negative() {
                return Err(Error::validation(format!("line {} has a negative price", item.id)));
            }
        }
        for group in &self.fulfillment_groups {
            if group.shipping_price.is_negative() {
                return Err(Error::validation(format!(
                    "fulfillment group {} has a negative shipping price",
                    group.id
                )));
            }
            for line in &group.items {
                if !self.items.iter().any(|item| &item.id == line) {
                    return Err(Error::validation(format!(
                        "fulfillment group {} references unknown line {line}",
                        group.id
                    )));
                }
            }
        }
        // Every physical item must belong to exactly one group once grouped.
        if !self.fulfillment_groups.is_empty() {
            for item in &self.items {
                let count = self
                    .fulfillment_groups
                    .iter()
                    .filter(|group| group.items.contains(&item.id))
                    .count();
                if count != 1 {
                    return Err(Error::validation(format!(
                        "line {} belongs to {count} fulfillment groups, expected exactly 1",
                        item.id
                    )));
                }
            }
        }
        Ok(())
    }

    fn assert_currency(&self, amount: Money) -> Result<()> {
        if amount.currency() == self.currency {
            Ok(())
        } else {
            Err(Error::CurrencyMismatch { expected: self.currency, actual: amount.currency() })
        }
    }

    fn touch(&mut self) {
        self.version = self.version.saturating_add(1);
        self.updated_at = Utc::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn sample_item(shop: &str, sku: &str, price: i64, qty: u32) -> CartItem {
        CartItem::new(shop, sku, sku, usd(price), qty).unwrap()
    }

    #[test]
    fn identical_lines_merge_but_different_ones_do_not() {
        let mut cart = Cart::new(Currency::USD);
        cart.add_item(sample_item("shop-1", "tee", 2_000, 1)).unwrap();
        cart.add_item(sample_item("shop-1", "tee", 2_000, 2)).unwrap();
        assert_eq!(cart.items.len(), 1);
        assert_eq!(cart.items[0].quantity, 3);

        cart.add_item(sample_item("shop-2", "tee", 2_000, 1)).unwrap();
        assert_eq!(cart.items.len(), 2, "different shops must not merge");

        let scheduled = sample_item("shop-1", "tee", 2_000, 1)
            .with_fulfillment(FulfillmentSelection::new(FulfillmentMethod::Pickup {
                location: Some("store-9".into()),
            }));
        cart.add_item(scheduled).unwrap();
        assert_eq!(cart.items.len(), 3, "different fulfilment must not merge");
    }

    #[test]
    fn currency_is_enforced() {
        let mut cart = Cart::new(Currency::USD);
        let eur = CartItem::new("s", "x", "x", Money::from_minor(100, Currency::EUR), 1).unwrap();
        assert!(matches!(cart.add_item(eur), Err(Error::CurrencyMismatch { .. })));
    }

    #[test]
    fn quantity_and_removal() {
        let mut cart = Cart::new(Currency::USD);
        let id = cart.add_item(sample_item("shop-1", "tee", 1_000, 1)).unwrap().id.clone();
        cart.set_quantity(&id, 5).unwrap();
        assert_eq!(cart.subtotal().unwrap(), usd(5_000));
        cart.set_quantity(&id, 0).unwrap();
        assert!(cart.items.is_empty());
        assert!(cart.set_quantity(&id, 1).is_err());
    }

    #[test]
    fn grouping_splits_by_shop_and_schedule() {
        let mut cart = Cart::new(Currency::USD);
        cart.add_item(sample_item("shop-1", "a", 1_000, 1)).unwrap();
        cart.add_item(sample_item("shop-2", "b", 1_000, 1)).unwrap();
        cart.add_item(
            sample_item("shop-1", "c", 1_000, 1)
                .with_fulfillment(FulfillmentSelection::new(FulfillmentMethod::Digital)),
        )
        .unwrap();

        cart.regroup_fulfillment(|key| {
            Ok(match key.method {
                FulfillmentMethod::Digital => usd(0),
                _ => usd(599),
            })
        })
        .unwrap();

        assert_eq!(cart.fulfillment_groups.len(), 3);
        assert!(cart.items.iter().all(|item| item.fulfillment_group_id.is_some()));
        cart.validate().unwrap();

        // Regrouping is idempotent.
        let before = cart.fulfillment_groups.clone();
        cart.regroup_fulfillment(|key| {
            Ok(match key.method {
                FulfillmentMethod::Digital => usd(0),
                _ => usd(599),
            })
        })
        .unwrap();
        assert_eq!(before, cart.fulfillment_groups);
    }

    #[test]
    fn removing_an_item_prunes_its_group() {
        let mut cart = Cart::new(Currency::USD);
        let id = cart.add_item(sample_item("shop-1", "a", 1_000, 1)).unwrap().id.clone();
        cart.add_item(sample_item("shop-2", "b", 1_000, 1)).unwrap();
        cart.regroup_fulfillment(|_| Ok(usd(0))).unwrap();
        assert_eq!(cart.fulfillment_groups.len(), 2);
        cart.remove_item(&id).unwrap();
        assert_eq!(cart.fulfillment_groups.len(), 1);
        cart.validate().unwrap();
    }

    #[test]
    fn version_increments_on_mutation() {
        let mut cart = Cart::new(Currency::USD);
        assert_eq!(cart.version, 0);
        cart.add_item(sample_item("shop-1", "a", 100, 1)).unwrap();
        assert_eq!(cart.version, 1);
        cart.apply_discount_code("SAVE10");
        cart.apply_discount_code("save10"); // duplicate, ignored
        assert_eq!(cart.discount_codes.len(), 1);
        assert_eq!(cart.version, 2);
    }

    #[test]
    fn empty_cart_fails_validation() {
        assert!(Cart::new(Currency::USD).validate().is_err());
    }

    #[test]
    fn multi_shop_detection() {
        let mut cart = Cart::new(Currency::USD);
        cart.add_item(sample_item("shop-1", "a", 100, 1)).unwrap();
        assert!(!cart.is_multi_shop());
        cart.add_item(sample_item("shop-2", "b", 100, 1)).unwrap();
        assert!(cart.is_multi_shop());
        assert_eq!(cart.unit_count(), 2);
    }
}
