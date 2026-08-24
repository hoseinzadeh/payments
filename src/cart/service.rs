//! Cart application service: repository-backed cart operations.

use std::sync::Arc;

use crate::address::Address;
use crate::cart::{Cart, CartItem, FulfillmentKey};
use crate::error::{Error, Result};
use crate::ids::{CartId, CustomerId, LineItemId};
use crate::money::{Currency, Money};
use crate::storage::CartRepository;

/// Decides the shipping charge for a shipment.
///
/// A closure is enough for flat and per-shop rates; wrap a carrier-rating API
/// in a struct for anything more involved.
pub trait ShippingRates: Send + Sync {
    /// The charge for one shipment.
    fn rate_for(&self, key: &FulfillmentKey, currency: Currency) -> Result<Money>;
}

/// Charges the same amount for every shipment.
#[derive(Debug, Clone)]
pub struct FlatShippingRate(pub Money);

impl ShippingRates for FlatShippingRate {
    fn rate_for(&self, _key: &FulfillmentKey, currency: Currency) -> Result<Money> {
        if self.0.currency() != currency {
            return Err(Error::CurrencyMismatch { expected: currency, actual: self.0.currency() });
        }
        Ok(self.0)
    }
}

/// Never charges for shipping.
#[derive(Debug, Clone, Copy, Default)]
pub struct FreeShipping;

impl ShippingRates for FreeShipping {
    fn rate_for(&self, _key: &FulfillmentKey, currency: Currency) -> Result<Money> {
        Ok(Money::zero(currency))
    }
}

impl<F> ShippingRates for F
where
    F: Fn(&FulfillmentKey, Currency) -> Result<Money> + Send + Sync,
{
    fn rate_for(&self, key: &FulfillmentKey, currency: Currency) -> Result<Money> {
        self(key, currency)
    }
}

/// Loads, mutates and persists carts.
///
/// Every mutating method re-reads the cart, applies the change and saves it, so
/// a concurrent modification surfaces as [`Error::Conflict`] rather than a lost
/// update.
#[derive(Clone)]
pub struct CartService {
    carts: Arc<dyn CartRepository>,
    shipping: Arc<dyn ShippingRates>,
}

impl CartService {
    /// Build a service with free shipping.
    pub fn new(carts: Arc<dyn CartRepository>) -> Self {
        Self { carts, shipping: Arc::new(FreeShipping) }
    }

    /// Build a service with a shipping-rate source.
    pub fn with_shipping(carts: Arc<dyn CartRepository>, shipping: Arc<dyn ShippingRates>) -> Self {
        Self { carts, shipping }
    }

    /// Create and persist an empty cart.
    pub async fn create(
        &self,
        customer_id: Option<CustomerId>,
        currency: Currency,
    ) -> Result<Cart> {
        let mut cart = Cart::new(currency);
        cart.customer_id = customer_id;
        cart.version = 1;
        self.carts.save(&cart).await?;
        Ok(cart)
    }

    /// Load a cart or fail with a precise not-found error.
    pub async fn get(&self, id: &CartId) -> Result<Cart> {
        self.carts
            .get(id)
            .await?
            .ok_or_else(|| Error::not_found(CartId::kind(), id))
    }

    /// Add an item and re-group shipments.
    pub async fn add_item(&self, id: &CartId, item: CartItem) -> Result<Cart> {
        self.mutate(id, |cart| {
            cart.add_item(item.clone())?;
            Ok(())
        })
        .await
    }

    /// Change a line's quantity; zero removes the line.
    pub async fn set_quantity(
        &self,
        id: &CartId,
        line: &LineItemId,
        quantity: u32,
    ) -> Result<Cart> {
        self.mutate(id, |cart| cart.set_quantity(line, quantity)).await
    }

    /// Remove a line.
    pub async fn remove_item(&self, id: &CartId, line: &LineItemId) -> Result<Cart> {
        self.mutate(id, |cart| cart.remove_item(line)).await
    }

    /// Empty the cart.
    pub async fn clear(&self, id: &CartId) -> Result<Cart> {
        self.mutate(id, |cart| {
            cart.clear();
            Ok(())
        })
        .await
    }

    /// Apply a promotion code.
    pub async fn apply_discount_code(&self, id: &CartId, code: &str) -> Result<Cart> {
        self.mutate(id, |cart| {
            cart.apply_discount_code(code);
            Ok(())
        })
        .await
    }

    /// Remove a promotion code.
    pub async fn remove_discount_code(&self, id: &CartId, code: &str) -> Result<Cart> {
        self.mutate(id, |cart| {
            cart.remove_discount_code(code);
            Ok(())
        })
        .await
    }

    /// Stage a gift card for redemption at checkout.
    pub async fn add_gift_card(&self, id: &CartId, code: &str) -> Result<Cart> {
        self.mutate(id, |cart| {
            cart.add_gift_card(code);
            Ok(())
        })
        .await
    }

    /// Choose whether the shopper's shop credit is spent at checkout.
    pub async fn set_apply_shop_credit(&self, id: &CartId, apply: bool) -> Result<Cart> {
        self.mutate(id, |cart| {
            cart.set_apply_shop_credit(apply);
            Ok(())
        })
        .await
    }

    /// Set the destination address and re-rate shipping.
    pub async fn set_shipping_address(&self, id: &CartId, address: Address) -> Result<Cart> {
        self.mutate(id, |cart| {
            cart.set_shipping_address(address.clone());
            Ok(())
        })
        .await
    }

    /// Delete a cart.
    pub async fn delete(&self, id: &CartId) -> Result<()> {
        self.carts.delete(id).await
    }

    /// Re-group shipments and re-rate shipping without other changes.
    pub async fn regroup(&self, id: &CartId) -> Result<Cart> {
        self.mutate(id, |_| Ok(())).await
    }

    async fn mutate<F>(&self, id: &CartId, apply: F) -> Result<Cart>
    where
        F: FnOnce(&mut Cart) -> Result<()>,
    {
        let mut cart = self.get(id).await?;
        apply(&mut cart)?;
        if !cart.items.is_empty() {
            let currency = cart.currency;
            let shipping = self.shipping.clone();
            cart.regroup_fulfillment(|key| shipping.rate_for(key, currency))?;
        } else {
            cart.fulfillment_groups.clear();
        }
        self.carts.save(&cart).await?;
        Ok(cart)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::CountryCode;
    use crate::storage::memory::InMemoryCartRepository;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    fn service() -> CartService {
        CartService::with_shipping(
            Arc::new(InMemoryCartRepository::new()),
            Arc::new(FlatShippingRate(usd(599))),
        )
    }

    #[tokio::test]
    async fn full_cart_lifecycle() {
        let service = service();
        let cart = service.create(Some(CustomerId::new()), Currency::USD).await.unwrap();

        let cart = service
            .add_item(
                &cart.id,
                CartItem::new("shop-1", "tee", "T-shirt", usd(2_000), 2).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cart.subtotal().unwrap(), usd(4_000));
        assert_eq!(cart.fulfillment_groups.len(), 1);
        assert_eq!(cart.fulfillment_groups[0].shipping_price, usd(599));

        let line = cart.items[0].id.clone();
        let cart = service.set_quantity(&cart.id, &line, 5).await.unwrap();
        assert_eq!(cart.subtotal().unwrap(), usd(10_000));

        let cart = service.apply_discount_code(&cart.id, "SAVE10").await.unwrap();
        assert_eq!(cart.discount_codes, vec!["SAVE10".to_owned()]);

        let cart = service
            .set_shipping_address(&cart.id, Address::new(CountryCode::US).with_region("CA"))
            .await
            .unwrap();
        assert!(cart.shipping_address.is_some());
        assert!(cart.fulfillment_groups[0].destination.is_some());

        let cart = service.remove_item(&cart.id, &line).await.unwrap();
        assert!(cart.items.is_empty());
        assert!(cart.fulfillment_groups.is_empty());

        service.delete(&cart.id).await.unwrap();
        assert!(service.get(&cart.id).await.is_err());
    }

    #[tokio::test]
    async fn shipping_is_rated_per_shipment() {
        let service = service();
        let cart = service.create(None, Currency::USD).await.unwrap();
        service
            .add_item(&cart.id, CartItem::new("shop-1", "a", "A", usd(1_000), 1).unwrap())
            .await
            .unwrap();
        let cart = service
            .add_item(&cart.id, CartItem::new("shop-2", "b", "B", usd(1_000), 1).unwrap())
            .await
            .unwrap();

        assert_eq!(cart.fulfillment_groups.len(), 2, "one shipment per shop");
        let shipping =
            Money::sum(cart.fulfillment_groups.iter().map(|g| g.shipping_price), Currency::USD)
                .unwrap();
        assert_eq!(shipping, usd(1_198));
    }

    #[tokio::test]
    async fn unknown_carts_report_not_found() {
        let service = service();
        let error = service.get(&CartId::from_string("cart_missing")).await.unwrap_err();
        assert!(matches!(error, Error::NotFound { .. }));
    }

    #[tokio::test]
    async fn closure_shipping_rates_work() {
        let repository = Arc::new(InMemoryCartRepository::new());
        let rates = |key: &FulfillmentKey, currency: Currency| -> Result<Money> {
            Ok(if key.shop_id.as_str() == "shop-free" {
                Money::zero(currency)
            } else {
                Money::from_minor(1_000, currency)
            })
        };
        let service = CartService::with_shipping(repository, Arc::new(rates));

        let cart = service.create(None, Currency::USD).await.unwrap();
        service
            .add_item(&cart.id, CartItem::new("shop-free", "a", "A", usd(100), 1).unwrap())
            .await
            .unwrap();
        let cart = service
            .add_item(&cart.id, CartItem::new("shop-paid", "b", "B", usd(100), 1).unwrap())
            .await
            .unwrap();

        let free = cart
            .fulfillment_groups
            .iter()
            .find(|group| group.shop_id.as_str() == "shop-free")
            .unwrap();
        assert!(free.shipping_price.is_zero());
    }
}
