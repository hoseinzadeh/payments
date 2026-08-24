//! Typed identifiers.
//!
//! Every entity gets its own ID type so that an `OrderId` can never be passed
//! where a `CartId` is expected. All of them are thin wrappers around a
//! `String`, which keeps them compatible with externally-generated identifiers
//! (a shop ID from your own database, a Stripe customer ID, …) while still
//! offering [`new`](CartId::new) for freshly generated UUIDs.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! typed_id {
    ($(#[$meta:meta])* $name:ident, $prefix:literal, $kind:literal) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Generate a new random identifier with the conventional prefix.
            pub fn new() -> Self {
                Self(format!("{}_{}", $prefix, uuid::Uuid::new_v4().simple()))
            }

            /// Wrap an existing identifier, e.g. one owned by your own database.
            pub fn from_string(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// The identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// The entity kind, used in error messages.
            pub const fn kind() -> &'static str {
                $kind
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }
    };
}

typed_id!(
    /// Identifies a shopping cart.
    CartId, "cart", "cart");
typed_id!(
    /// Identifies a line item inside a cart or order.
    LineItemId, "li", "line item");
typed_id!(
    /// Identifies an order.
    OrderId, "ord", "order");
typed_id!(
    /// Identifies a buyer.
    CustomerId, "cus", "customer");
typed_id!(
    /// Identifies a shop / merchant / vendor selling through the platform.
    ShopId, "shop", "shop");
typed_id!(
    /// Identifies an account that can receive money (a shop, the platform, a subsidiser).
    AccountId, "acct", "account");
typed_id!(
    /// Identifies a payment attempt (authorisation + captures).
    PaymentId, "pay", "payment");
typed_id!(
    /// Identifies a single capture of an authorisation.
    CaptureId, "cap", "capture");
typed_id!(
    /// Identifies a refund.
    RefundId, "re", "refund");
typed_id!(
    /// Identifies a dispute / chargeback.
    DisputeId, "dp", "dispute");
typed_id!(
    /// Identifies a stored payment instrument token (never card data itself).
    PaymentMethodId, "pm", "payment method");
typed_id!(
    /// Identifies a discount or promotion definition.
    DiscountId, "disc", "discount");
typed_id!(
    /// Identifies a gift card.
    GiftCardId, "gc", "gift card");
typed_id!(
    /// Identifies a fulfilment group (one shipment / delivery slot).
    FulfillmentGroupId, "ful", "fulfillment group");
typed_id!(
    /// Identifies a webhook or domain event.
    EventId, "evt", "event");
typed_id!(
    /// Identifies a ledger entry in the shop-credit ledger.
    LedgerEntryId, "led", "ledger entry");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_prefixed_unique_and_serde_transparent() {
        let a = OrderId::new();
        let b = OrderId::new();
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("ord_"));
        assert_eq!(serde_json::to_string(&a).unwrap(), format!("\"{a}\""));

        let external = ShopId::from_string("shop-42");
        assert_eq!(external.as_str(), "shop-42");
        assert_eq!(ShopId::kind(), "shop");
    }
}
