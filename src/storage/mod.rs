//! Storage abstraction.
//!
//! The engine never talks to a database directly; it talks to the repository
//! traits in this module. Implement them over Postgres, DynamoDB, Redis or
//! anything else, and the rest of the crate is unchanged. A complete in-memory
//! implementation lives in [`memory`] behind the `memory-store` feature and is
//! what the tests and examples use.
//!
//! # Concurrency
//!
//! Every mutable aggregate carries a `version` that it bumps on each mutation.
//! `save` must reject a write whose version is not strictly greater than the
//! stored one, which turns a lost update into an
//! [`Error::Conflict`](crate::error::Error::Conflict) instead of silently
//! overwriting a concurrent change.

#[cfg(feature = "memory-store")]
pub mod memory;

use async_trait::async_trait;

use crate::cart::Cart;
use crate::error::Result;
use crate::ids::{
    CartId, CustomerId, DisputeId, GiftCardId, OrderId, PaymentId, PaymentMethodId, ShopId,
};
use crate::ledger::{GiftCard, ShopCreditAccount};
use crate::money::Currency;
use crate::order::Order;
use crate::payment::dispute::Dispute;
use crate::payment::method::PaymentMethodRef;
use crate::payment::Payment;
use crate::pricing::Discount;

/// Persistence for shopping carts.
#[async_trait]
pub trait CartRepository: Send + Sync {
    /// Load a cart.
    async fn get(&self, id: &CartId) -> Result<Option<Cart>>;
    /// Insert or update a cart, enforcing the version check.
    async fn save(&self, cart: &Cart) -> Result<()>;
    /// Delete a cart.
    async fn delete(&self, id: &CartId) -> Result<()>;
    /// All carts belonging to a customer.
    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<Cart>>;
}

/// Persistence for orders.
#[async_trait]
pub trait OrderRepository: Send + Sync {
    /// Load an order.
    async fn get(&self, id: &OrderId) -> Result<Option<Order>>;
    /// Insert or update an order, enforcing the version check.
    async fn save(&self, order: &Order) -> Result<()>;
    /// All orders belonging to a customer, newest first.
    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<Order>>;
    /// All orders containing items from a shop, newest first.
    async fn list_for_shop(&self, shop_id: &ShopId) -> Result<Vec<Order>>;
    /// Every order, for reporting. Implementations should paginate in practice.
    async fn list_all(&self) -> Result<Vec<Order>>;
}

/// Persistence for payment attempts.
#[async_trait]
pub trait PaymentRepository: Send + Sync {
    /// Load a payment.
    async fn get(&self, id: &PaymentId) -> Result<Option<Payment>>;
    /// Insert or update a payment.
    async fn save(&self, payment: &Payment) -> Result<()>;
    /// All payments for an order.
    async fn list_for_order(&self, order_id: &OrderId) -> Result<Vec<Payment>>;
    /// Find a payment by the gateway's transaction id. Used by webhooks.
    async fn find_by_transaction(&self, transaction_id: &str) -> Result<Option<Payment>>;
    /// Authorisations that lapse before `before`, so they can be captured or released.
    async fn list_expiring(
        &self,
        before: chrono::DateTime<chrono::Utc>,
    ) -> Result<Vec<Payment>>;
}

/// Persistence for vaulted instruments ("card on file").
#[async_trait]
pub trait PaymentMethodRepository: Send + Sync {
    /// Load an instrument.
    async fn get(&self, id: &PaymentMethodId) -> Result<Option<PaymentMethodRef>>;
    /// Store an instrument.
    async fn save(&self, method: &PaymentMethodRef) -> Result<()>;
    /// Forget an instrument.
    async fn delete(&self, id: &PaymentMethodId) -> Result<()>;
    /// A customer's instruments.
    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<PaymentMethodRef>>;
    /// The customer's default instrument, if they have one.
    async fn default_for_customer(
        &self,
        customer_id: &CustomerId,
    ) -> Result<Option<PaymentMethodRef>> {
        Ok(self
            .list_for_customer(customer_id)
            .await?
            .into_iter()
            .find(|method| method.is_default))
    }
}

/// Persistence for promotions.
#[async_trait]
pub trait DiscountRepository: Send + Sync {
    /// Resolve a promotion code the shopper typed. Case-insensitive.
    async fn find_by_code(&self, code: &str) -> Result<Option<Discount>>;
    /// Promotions that apply without a code.
    async fn list_automatic(&self) -> Result<Vec<Discount>>;
    /// Store a promotion.
    async fn save(&self, discount: &Discount) -> Result<()>;
    /// Record that a promotion was used, for redemption caps.
    async fn increment_redemptions(&self, id: &crate::ids::DiscountId) -> Result<()>;
}

/// Persistence for shop credit balances.
#[async_trait]
pub trait ShopCreditRepository: Send + Sync {
    /// Load a customer's balance at a shop.
    async fn get(
        &self,
        customer_id: &CustomerId,
        shop_id: &ShopId,
        currency: Currency,
    ) -> Result<Option<ShopCreditAccount>>;
    /// Store a balance.
    async fn save(&self, account: &ShopCreditAccount) -> Result<()>;
    /// Every shop where a customer has credit.
    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<ShopCreditAccount>>;
}

/// Persistence for gift cards.
#[async_trait]
pub trait GiftCardRepository: Send + Sync {
    /// Load by id.
    async fn get(&self, id: &GiftCardId) -> Result<Option<GiftCard>>;
    /// Resolve a code the shopper typed. Implementations must look the card up
    /// by *hash*; the plaintext code is never stored.
    async fn find_by_code(&self, code: &str) -> Result<Option<GiftCard>>;
    /// Store a card.
    async fn save(&self, card: &GiftCard) -> Result<()>;
}

/// Persistence for disputes.
#[async_trait]
pub trait DisputeRepository: Send + Sync {
    /// Load a dispute.
    async fn get(&self, id: &DisputeId) -> Result<Option<Dispute>>;
    /// Store a dispute.
    async fn save(&self, dispute: &Dispute) -> Result<()>;
    /// Disputes raised against an order.
    async fn list_for_order(&self, order_id: &OrderId) -> Result<Vec<Dispute>>;
    /// Every dispute, for reporting.
    async fn list_all(&self) -> Result<Vec<Dispute>>;
}

/// The state of an idempotent operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdempotencyOutcome {
    /// No previous attempt; the caller owns this key and should proceed.
    Started,
    /// A previous attempt is still running. The caller should not proceed.
    InFlight,
    /// A previous attempt finished; here is what it returned.
    Completed(String),
}

/// Deduplication for client-supplied idempotency keys.
///
/// The `fingerprint` is a hash of the request body. Reusing a key with a
/// different fingerprint is a client bug and must fail loudly with
/// [`Error::IdempotencyConflict`](crate::error::Error::IdempotencyConflict)
/// rather than replaying an unrelated response.
#[async_trait]
pub trait IdempotencyStore: Send + Sync {
    /// Claim a key.
    async fn begin(&self, key: &str, fingerprint: &str) -> Result<IdempotencyOutcome>;
    /// Record the successful result for a key.
    async fn complete(&self, key: &str, response: &str) -> Result<()>;
    /// Release a key after a failure so the operation can be retried.
    async fn abort(&self, key: &str) -> Result<()>;
}

/// Deduplication for inbound webhook deliveries.
#[async_trait]
pub trait ProcessedEventStore: Send + Sync {
    /// Record an event id. Returns `true` if this is the first time it is seen.
    async fn mark_processed(&self, key: &str) -> Result<bool>;
    /// Forget an event id so a failed delivery can be retried.
    async fn unmark(&self, key: &str) -> Result<()>;
}
