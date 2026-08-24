//! Fulfilment groups: how the items of one cart are physically delivered.
//!
//! A single cart routinely needs several shipments: items from different shops,
//! shipped from different warehouses, or scheduled for different delivery
//! windows. Each such shipment is a [`FulfillmentGroup`], and shipping charges,
//! shipping tax and delivery-time capture all operate per group rather than per
//! order.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::address::Address;
use crate::ids::{FulfillmentGroupId, LineItemId, ShopId};
use crate::money::Money;
use crate::pricing::TaxCode;

/// How an item reaches the customer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FulfillmentMethod {
    /// Parcel shipping through a carrier.
    Shipping {
        /// Carrier name, e.g. `"ups"`.
        carrier: String,
        /// Service level, e.g. `"ground"`.
        service: String,
    },
    /// Courier delivery, typically same-day, within a time window.
    LocalDelivery {
        /// Free-form window label, e.g. `"18:00-20:00"`.
        window: Option<String>,
    },
    /// Customer collects the goods.
    Pickup {
        /// Identifier of the pickup location.
        location: Option<String>,
    },
    /// No physical delivery (downloads, licences, services).
    Digital,
}

impl FulfillmentMethod {
    /// Stable discriminant used when grouping items.
    pub fn kind(&self) -> &'static str {
        match self {
            FulfillmentMethod::Shipping { .. } => "shipping",
            FulfillmentMethod::LocalDelivery { .. } => "local_delivery",
            FulfillmentMethod::Pickup { .. } => "pickup",
            FulfillmentMethod::Digital => "digital",
        }
    }

    /// Whether funds should normally only be captured once the goods move.
    ///
    /// Delivery-style fulfilment is the canonical case for authorise-now /
    /// capture-on-delivery, whereas digital goods can be captured immediately.
    pub fn prefers_delayed_capture(&self) -> bool {
        !matches!(self, FulfillmentMethod::Digital)
    }
}

impl fmt::Display for FulfillmentMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FulfillmentMethod::Shipping { carrier, service } => {
                write!(f, "shipping:{carrier}/{service}")
            }
            FulfillmentMethod::LocalDelivery { window } => {
                write!(f, "local_delivery:{}", window.as_deref().unwrap_or("any"))
            }
            FulfillmentMethod::Pickup { location } => {
                write!(f, "pickup:{}", location.as_deref().unwrap_or("default"))
            }
            FulfillmentMethod::Digital => f.write_str("digital"),
        }
    }
}

/// What a buyer chose for a particular line item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FulfillmentSelection {
    /// Delivery mechanism.
    pub method: FulfillmentMethod,
    /// Origin location / warehouse. Items shipping from different origins are
    /// never merged into one shipment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Requested delivery or pickup time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_for: Option<DateTime<Utc>>,
}

impl FulfillmentSelection {
    /// A selection with only a method.
    pub fn new(method: FulfillmentMethod) -> Self {
        Self { method, origin: None, scheduled_for: None }
    }

    /// Builder: set the origin location.
    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.origin = Some(origin.into());
        self
    }

    /// Builder: set the requested delivery time.
    pub fn with_schedule(mut self, at: DateTime<Utc>) -> Self {
        self.scheduled_for = Some(at);
        self
    }

    /// The grouping key for a selection made by `shop`.
    pub fn key(&self, shop: &ShopId) -> FulfillmentKey {
        FulfillmentKey {
            shop_id: shop.clone(),
            method: self.method.clone(),
            origin: self.origin.clone(),
            scheduled_for: self.scheduled_for,
        }
    }
}

impl Default for FulfillmentSelection {
    fn default() -> Self {
        Self::new(FulfillmentMethod::Shipping {
            carrier: "standard".to_owned(),
            service: "ground".to_owned(),
        })
    }
}

/// The identity of a shipment: same shop, same method, same origin, same slot.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FulfillmentKey {
    /// Shop that ships the goods.
    pub shop_id: ShopId,
    /// Delivery mechanism.
    pub method: FulfillmentMethod,
    /// Origin location.
    pub origin: Option<String>,
    /// Scheduled time.
    pub scheduled_for: Option<DateTime<Utc>>,
}

impl FulfillmentKey {
    /// A deterministic group id, so that repeated pricing of an unchanged cart
    /// produces byte-identical quotes (important for idempotency and caching).
    pub fn deterministic_id(&self) -> FulfillmentGroupId {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        FulfillmentGroupId::from_string(format!("ful_{:016x}", hasher.finish()))
    }
}

/// One shipment within a cart or order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FulfillmentGroup {
    /// Identifier, usually derived from [`FulfillmentKey::deterministic_id`].
    pub id: FulfillmentGroupId,
    /// Shop responsible for the shipment.
    pub shop_id: ShopId,
    /// Delivery mechanism.
    pub method: FulfillmentMethod,
    /// Origin location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// Scheduled delivery or pickup time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheduled_for: Option<DateTime<Utc>>,
    /// Destination address; falls back to the cart's shipping address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<Address>,
    /// Shipping charge for the whole group.
    pub shipping_price: Money,
    /// Tax code applied to the shipping charge.
    #[serde(default)]
    pub shipping_tax_code: TaxCode,
    /// Items included in this shipment.
    pub items: Vec<LineItemId>,
    /// Current progress of the shipment.
    #[serde(default)]
    pub status: FulfillmentStatus,
}

impl FulfillmentGroup {
    /// Build an empty group from a key and a shipping charge.
    pub fn from_key(key: FulfillmentKey, shipping_price: Money) -> Self {
        Self {
            id: key.deterministic_id(),
            shop_id: key.shop_id,
            method: key.method,
            origin: key.origin,
            scheduled_for: key.scheduled_for,
            destination: None,
            shipping_price,
            shipping_tax_code: TaxCode::shipping(),
            items: Vec::new(),
            status: FulfillmentStatus::Pending,
        }
    }
}

/// Lifecycle of a shipment. Capture of funds is typically tied to `Delivered`
/// (or `Shipped`, depending on the merchant's policy).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FulfillmentStatus {
    /// Not yet handed to the carrier.
    #[default]
    Pending,
    /// Being picked and packed.
    Processing,
    /// Handed over to the carrier / courier.
    Shipped,
    /// Received by the customer. Usual trigger for capture.
    Delivered,
    /// Could not be delivered and came back.
    Returned,
    /// Cancelled before dispatch.
    Canceled,
}

impl FulfillmentStatus {
    /// Whether the group can still transition to `next`.
    pub fn can_transition_to(self, next: FulfillmentStatus) -> bool {
        use FulfillmentStatus::*;
        matches!(
            (self, next),
            (Pending, Processing)
                | (Pending, Canceled)
                | (Processing, Shipped)
                | (Processing, Canceled)
                | (Shipped, Delivered)
                | (Shipped, Returned)
                | (Delivered, Returned)
        )
    }

    /// Terminal states no longer change.
    pub fn is_terminal(self) -> bool {
        matches!(self, FulfillmentStatus::Returned | FulfillmentStatus::Canceled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Currency;

    #[test]
    fn group_ids_are_stable_and_discriminating() {
        let shop = ShopId::from_string("shop-1");
        let selection = FulfillmentSelection::new(FulfillmentMethod::LocalDelivery {
            window: Some("18:00-20:00".into()),
        })
        .with_origin("warehouse-a");

        let a = selection.key(&shop).deterministic_id();
        let b = selection.key(&shop).deterministic_id();
        assert_eq!(a, b, "same key must produce the same id");

        let other_origin = selection.clone().with_origin("warehouse-b");
        assert_ne!(a, other_origin.key(&shop).deterministic_id());

        let other_shop = selection.key(&ShopId::from_string("shop-2")).deterministic_id();
        assert_ne!(a, other_shop);
    }

    #[test]
    fn digital_goods_capture_immediately() {
        assert!(!FulfillmentMethod::Digital.prefers_delayed_capture());
        assert!(FulfillmentMethod::Pickup { location: None }.prefers_delayed_capture());
    }

    #[test]
    fn status_machine_rejects_illegal_jumps() {
        assert!(FulfillmentStatus::Pending.can_transition_to(FulfillmentStatus::Processing));
        assert!(!FulfillmentStatus::Pending.can_transition_to(FulfillmentStatus::Delivered));
        assert!(FulfillmentStatus::Canceled.is_terminal());
    }

    #[test]
    fn group_from_key_defaults_to_shipping_tax_code() {
        let key = FulfillmentSelection::default().key(&ShopId::from_string("s"));
        let group = FulfillmentGroup::from_key(key, Money::from_minor(500, Currency::USD));
        assert_eq!(group.shipping_tax_code, TaxCode::shipping());
        assert!(group.items.is_empty());
    }
}
