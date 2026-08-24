//! In-memory reference implementations of every repository trait.
//!
//! These are complete and correct — including the optimistic-concurrency
//! checks — but they are not durable. They exist so that tests, examples and
//! local development need no database, and so that a new backend has a working
//! specification to compare against.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::RwLock;

use crate::cart::Cart;
use crate::error::{Error, Result};
use crate::ids::{
    CartId, CustomerId, DiscountId, DisputeId, GiftCardId, OrderId, PaymentId, PaymentMethodId,
    ShopId,
};
use crate::ledger::{GiftCard, ShopCreditAccount, hash_gift_card_code};
use crate::money::Currency;
use crate::order::Order;
use crate::payment::Payment;
use crate::payment::dispute::Dispute;
use crate::payment::method::PaymentMethodRef;
use crate::pricing::Discount;
use crate::storage::{
    CartRepository, DiscountRepository, DisputeRepository, GiftCardRepository, IdempotencyOutcome,
    IdempotencyStore, OrderRepository, PaymentMethodRepository, PaymentRepository,
    ProcessedEventStore, ShopCreditRepository,
};

/// A poisoned lock means another thread panicked while holding it; the data may
/// be inconsistent, so surface it rather than papering over it.
fn poisoned<T>(_: T) -> Error {
    Error::storage("in-memory store lock was poisoned by a panicking thread")
}

fn check_version(kind: &'static str, id: String, stored: Option<u64>, incoming: u64) -> Result<()> {
    if let Some(stored) = stored
        && incoming <= stored
    {
        return Err(Error::Conflict {
            kind,
            id,
            message: format!(
                "stale write: stored version is {stored}, attempted to save version {incoming}"
            ),
        });
    }
    Ok(())
}

macro_rules! simple_store {
    ($name:ident, $value:ty) => {
        /// In-memory store.
        #[derive(Debug, Default)]
        pub struct $name {
            items: RwLock<HashMap<String, $value>>,
        }

        impl $name {
            /// An empty store.
            pub fn new() -> Self {
                Self { items: RwLock::new(HashMap::new()) }
            }

            /// Number of stored entities.
            pub fn len(&self) -> usize {
                self.items.read().map(|items| items.len()).unwrap_or(0)
            }

            /// Whether the store is empty.
            pub fn is_empty(&self) -> bool {
                self.len() == 0
            }
        }
    };
}

simple_store!(InMemoryCartRepository, Cart);
simple_store!(InMemoryOrderRepository, Order);
simple_store!(InMemoryPaymentRepository, Payment);
simple_store!(InMemoryPaymentMethodRepository, PaymentMethodRef);
simple_store!(InMemoryDisputeRepository, Dispute);
simple_store!(InMemoryGiftCardRepository, GiftCard);
simple_store!(InMemoryShopCreditRepository, ShopCreditAccount);
simple_store!(InMemoryDiscountRepository, Discount);

#[async_trait]
impl CartRepository for InMemoryCartRepository {
    async fn get(&self, id: &CartId) -> Result<Option<Cart>> {
        Ok(self.items.read().map_err(poisoned)?.get(id.as_str()).cloned())
    }

    async fn save(&self, cart: &Cart) -> Result<()> {
        let mut items = self.items.write().map_err(poisoned)?;
        let stored = items.get(cart.id.as_str()).map(|existing| existing.version);
        check_version(CartId::kind(), cart.id.to_string(), stored, cart.version)?;
        items.insert(cart.id.to_string(), cart.clone());
        Ok(())
    }

    async fn delete(&self, id: &CartId) -> Result<()> {
        self.items.write().map_err(poisoned)?.remove(id.as_str());
        Ok(())
    }

    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<Cart>> {
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|cart| cart.customer_id.as_ref() == Some(customer_id))
            .cloned()
            .collect())
    }
}

#[async_trait]
impl OrderRepository for InMemoryOrderRepository {
    async fn get(&self, id: &OrderId) -> Result<Option<Order>> {
        Ok(self.items.read().map_err(poisoned)?.get(id.as_str()).cloned())
    }

    async fn save(&self, order: &Order) -> Result<()> {
        let mut items = self.items.write().map_err(poisoned)?;
        let stored = items.get(order.id.as_str()).map(|existing| existing.version);
        check_version(OrderId::kind(), order.id.to_string(), stored, order.version)?;
        items.insert(order.id.to_string(), order.clone());
        Ok(())
    }

    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<Order>> {
        let mut orders: Vec<Order> = self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|order| order.customer_id.as_ref() == Some(customer_id))
            .cloned()
            .collect();
        orders.sort_by_key(|order| std::cmp::Reverse(order.created_at));
        Ok(orders)
    }

    async fn list_for_shop(&self, shop_id: &ShopId) -> Result<Vec<Order>> {
        let mut orders: Vec<Order> = self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|order| {
                order.quote.shop_totals.iter().any(|totals| &totals.shop_id == shop_id)
            })
            .cloned()
            .collect();
        orders.sort_by_key(|order| std::cmp::Reverse(order.created_at));
        Ok(orders)
    }

    async fn list_all(&self) -> Result<Vec<Order>> {
        let mut orders: Vec<Order> =
            self.items.read().map_err(poisoned)?.values().cloned().collect();
        orders.sort_by_key(|order| std::cmp::Reverse(order.created_at));
        Ok(orders)
    }
}

#[async_trait]
impl PaymentRepository for InMemoryPaymentRepository {
    async fn get(&self, id: &PaymentId) -> Result<Option<Payment>> {
        Ok(self.items.read().map_err(poisoned)?.get(id.as_str()).cloned())
    }

    async fn save(&self, payment: &Payment) -> Result<()> {
        let mut items = self.items.write().map_err(poisoned)?;
        let stored = items.get(payment.id.as_str()).map(|existing| existing.version);
        check_version(PaymentId::kind(), payment.id.to_string(), stored, payment.version)?;
        items.insert(payment.id.to_string(), payment.clone());
        Ok(())
    }

    async fn list_for_order(&self, order_id: &OrderId) -> Result<Vec<Payment>> {
        let mut payments: Vec<Payment> = self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|payment| &payment.order_id == order_id)
            .cloned()
            .collect();
        payments.sort_by_key(|payment| payment.created_at);
        Ok(payments)
    }

    async fn find_by_transaction(&self, transaction_id: &str) -> Result<Option<Payment>> {
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .find(|payment| payment.transaction_id.as_deref() == Some(transaction_id))
            .cloned())
    }

    async fn list_expiring(&self, before: DateTime<Utc>) -> Result<Vec<Payment>> {
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|payment| {
                payment.status.is_capturable()
                    && payment.authorization_expires_at.is_some_and(|at| at <= before)
            })
            .cloned()
            .collect())
    }
}

#[async_trait]
impl PaymentMethodRepository for InMemoryPaymentMethodRepository {
    async fn get(&self, id: &PaymentMethodId) -> Result<Option<PaymentMethodRef>> {
        Ok(self.items.read().map_err(poisoned)?.get(id.as_str()).cloned())
    }

    async fn save(&self, method: &PaymentMethodRef) -> Result<()> {
        let mut items = self.items.write().map_err(poisoned)?;
        // Only one default per customer.
        if method.is_default {
            for existing in items.values_mut() {
                if existing.customer_id == method.customer_id && existing.id != method.id {
                    existing.is_default = false;
                }
            }
        }
        items.insert(method.id.to_string(), method.clone());
        Ok(())
    }

    async fn delete(&self, id: &PaymentMethodId) -> Result<()> {
        self.items.write().map_err(poisoned)?.remove(id.as_str());
        Ok(())
    }

    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<PaymentMethodRef>> {
        let mut methods: Vec<PaymentMethodRef> = self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|method| &method.customer_id == customer_id)
            .cloned()
            .collect();
        methods.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.created_at.cmp(&b.created_at)));
        Ok(methods)
    }
}

#[async_trait]
impl DisputeRepository for InMemoryDisputeRepository {
    async fn get(&self, id: &DisputeId) -> Result<Option<Dispute>> {
        Ok(self.items.read().map_err(poisoned)?.get(id.as_str()).cloned())
    }

    async fn save(&self, dispute: &Dispute) -> Result<()> {
        self.items.write().map_err(poisoned)?.insert(dispute.id.to_string(), dispute.clone());
        Ok(())
    }

    async fn list_for_order(&self, order_id: &OrderId) -> Result<Vec<Dispute>> {
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|dispute| &dispute.order_id == order_id)
            .cloned()
            .collect())
    }

    async fn list_all(&self) -> Result<Vec<Dispute>> {
        Ok(self.items.read().map_err(poisoned)?.values().cloned().collect())
    }
}

#[async_trait]
impl GiftCardRepository for InMemoryGiftCardRepository {
    async fn get(&self, id: &GiftCardId) -> Result<Option<GiftCard>> {
        Ok(self.items.read().map_err(poisoned)?.get(id.as_str()).cloned())
    }

    async fn find_by_code(&self, code: &str) -> Result<Option<GiftCard>> {
        let hash = hash_gift_card_code(code);
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .find(|card| card.code_hash == hash)
            .cloned())
    }

    async fn save(&self, card: &GiftCard) -> Result<()> {
        let mut items = self.items.write().map_err(poisoned)?;
        let stored = items.get(card.id.as_str()).map(|existing| existing.version);
        check_version(GiftCardId::kind(), card.id.to_string(), stored, card.version)?;
        items.insert(card.id.to_string(), card.clone());
        Ok(())
    }
}

#[async_trait]
impl ShopCreditRepository for InMemoryShopCreditRepository {
    async fn get(
        &self,
        customer_id: &CustomerId,
        shop_id: &ShopId,
        currency: Currency,
    ) -> Result<Option<ShopCreditAccount>> {
        let key = credit_key(customer_id, shop_id, currency);
        Ok(self.items.read().map_err(poisoned)?.get(&key).cloned())
    }

    async fn save(&self, account: &ShopCreditAccount) -> Result<()> {
        let key = credit_key(&account.customer_id, &account.shop_id, account.currency);
        let mut items = self.items.write().map_err(poisoned)?;
        let stored = items.get(&key).map(|existing| existing.version);
        check_version("shop credit", key.clone(), stored, account.version)?;
        items.insert(key, account.clone());
        Ok(())
    }

    async fn list_for_customer(&self, customer_id: &CustomerId) -> Result<Vec<ShopCreditAccount>> {
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|account| &account.customer_id == customer_id)
            .cloned()
            .collect())
    }
}

fn credit_key(customer_id: &CustomerId, shop_id: &ShopId, currency: Currency) -> String {
    format!("{customer_id}|{shop_id}|{currency}")
}

#[async_trait]
impl DiscountRepository for InMemoryDiscountRepository {
    async fn find_by_code(&self, code: &str) -> Result<Option<Discount>> {
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .find(|discount| {
                discount.code.as_deref().is_some_and(|stored| stored.eq_ignore_ascii_case(code))
            })
            .cloned())
    }

    async fn list_automatic(&self) -> Result<Vec<Discount>> {
        Ok(self
            .items
            .read()
            .map_err(poisoned)?
            .values()
            .filter(|discount| discount.code.is_none())
            .cloned()
            .collect())
    }

    async fn save(&self, discount: &Discount) -> Result<()> {
        self.items.write().map_err(poisoned)?.insert(discount.id.to_string(), discount.clone());
        Ok(())
    }

    async fn increment_redemptions(&self, id: &DiscountId) -> Result<()> {
        let mut items = self.items.write().map_err(poisoned)?;
        match items.get_mut(id.as_str()) {
            Some(discount) => {
                discount.conditions.redemptions = discount.conditions.redemptions.saturating_add(1);
                Ok(())
            }
            None => Err(Error::not_found("discount", id)),
        }
    }
}

/// In-memory idempotency store.
#[derive(Debug, Default)]
pub struct InMemoryIdempotencyStore {
    entries: RwLock<HashMap<String, IdempotencyEntry>>,
}

#[derive(Debug, Clone)]
struct IdempotencyEntry {
    fingerprint: String,
    response: Option<String>,
}

impl InMemoryIdempotencyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self { entries: RwLock::new(HashMap::new()) }
    }
}

#[async_trait]
impl IdempotencyStore for InMemoryIdempotencyStore {
    async fn begin(&self, key: &str, fingerprint: &str) -> Result<IdempotencyOutcome> {
        let mut entries = self.entries.write().map_err(poisoned)?;
        match entries.get(key) {
            Some(entry) if entry.fingerprint != fingerprint => {
                Err(Error::IdempotencyConflict { key: key.to_owned() })
            }
            Some(entry) => match &entry.response {
                Some(response) => Ok(IdempotencyOutcome::Completed(response.clone())),
                None => Ok(IdempotencyOutcome::InFlight),
            },
            None => {
                entries.insert(
                    key.to_owned(),
                    IdempotencyEntry { fingerprint: fingerprint.to_owned(), response: None },
                );
                Ok(IdempotencyOutcome::Started)
            }
        }
    }

    async fn complete(&self, key: &str, response: &str) -> Result<()> {
        let mut entries = self.entries.write().map_err(poisoned)?;
        match entries.get_mut(key) {
            Some(entry) => {
                entry.response = Some(response.to_owned());
                Ok(())
            }
            None => Err(Error::not_found("idempotency key", key)),
        }
    }

    async fn abort(&self, key: &str) -> Result<()> {
        self.entries.write().map_err(poisoned)?.remove(key);
        Ok(())
    }
}

/// In-memory webhook deduplication store.
#[derive(Debug, Default)]
pub struct InMemoryProcessedEventStore {
    seen: RwLock<std::collections::HashSet<String>>,
}

impl InMemoryProcessedEventStore {
    /// An empty store.
    pub fn new() -> Self {
        Self { seen: RwLock::new(std::collections::HashSet::new()) }
    }
}

#[async_trait]
impl ProcessedEventStore for InMemoryProcessedEventStore {
    async fn mark_processed(&self, key: &str) -> Result<bool> {
        Ok(self.seen.write().map_err(poisoned)?.insert(key.to_owned()))
    }

    async fn unmark(&self, key: &str) -> Result<()> {
        self.seen.write().map_err(poisoned)?.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Money;

    #[tokio::test]
    async fn optimistic_concurrency_rejects_stale_writes() {
        let repository = InMemoryCartRepository::new();
        let mut cart = Cart::new(Currency::USD);
        cart.apply_discount_code("SAVE"); // version 1
        repository.save(&cart).await.unwrap();

        // Two readers both mutate the version-1 cart.
        let mut a = repository.get(&cart.id).await.unwrap().unwrap();
        let mut b = repository.get(&cart.id).await.unwrap().unwrap();
        a.apply_discount_code("A");
        b.apply_discount_code("B");

        repository.save(&a).await.unwrap();
        let error = repository.save(&b).await.unwrap_err();
        assert!(matches!(error, Error::Conflict { .. }));
    }

    #[tokio::test]
    async fn gift_cards_are_looked_up_by_hash_only() {
        let repository = InMemoryGiftCardRepository::new();
        let card =
            GiftCard::issue("GIFT-1111-2222", Money::from_minor(1_000, Currency::USD)).unwrap();
        repository.save(&card).await.unwrap();

        assert!(repository.find_by_code("gift11112222").await.unwrap().is_some());
        assert!(repository.find_by_code("GIFT-1111-2223").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn idempotency_keys_replay_and_detect_payload_changes() {
        let store = InMemoryIdempotencyStore::new();
        assert_eq!(store.begin("k", "fp").await.unwrap(), IdempotencyOutcome::Started);
        assert_eq!(store.begin("k", "fp").await.unwrap(), IdempotencyOutcome::InFlight);

        store.complete("k", "{\"ok\":true}").await.unwrap();
        assert_eq!(
            store.begin("k", "fp").await.unwrap(),
            IdempotencyOutcome::Completed("{\"ok\":true}".to_owned())
        );

        let error = store.begin("k", "different").await.unwrap_err();
        assert!(matches!(error, Error::IdempotencyConflict { .. }));

        store.abort("k").await.unwrap();
        assert_eq!(store.begin("k", "fp").await.unwrap(), IdempotencyOutcome::Started);
    }

    #[tokio::test]
    async fn only_one_default_payment_method_per_customer() {
        let repository = InMemoryPaymentMethodRepository::new();
        let customer = CustomerId::new();
        let mut first = PaymentMethodRef::new(
            customer.clone(),
            crate::gateway::GatewayId::from_static("mock"),
            "tok_1",
            crate::payment::method::PaymentMethodKind::Wallet { provider: "paypal".into() },
        );
        first.is_default = true;
        repository.save(&first).await.unwrap();

        let mut second = PaymentMethodRef::new(
            customer.clone(),
            crate::gateway::GatewayId::from_static("mock"),
            "tok_2",
            crate::payment::method::PaymentMethodKind::Wallet { provider: "paypal".into() },
        );
        second.is_default = true;
        repository.save(&second).await.unwrap();

        let methods = repository.list_for_customer(&customer).await.unwrap();
        assert_eq!(methods.iter().filter(|method| method.is_default).count(), 1);
        assert_eq!(repository.default_for_customer(&customer).await.unwrap().unwrap().id, second.id);
    }

    #[tokio::test]
    async fn processed_events_deduplicate() {
        let store = InMemoryProcessedEventStore::new();
        assert!(store.mark_processed("evt_1").await.unwrap());
        assert!(!store.mark_processed("evt_1").await.unwrap());
        store.unmark("evt_1").await.unwrap();
        assert!(store.mark_processed("evt_1").await.unwrap());
    }
}
