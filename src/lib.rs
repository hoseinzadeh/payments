//! A gateway-agnostic payments engine for marketplaces.
//!
//! This crate models the whole commerce money path — cart, pricing, tax,
//! discounts, checkout, authorisation, capture, split settlement, refunds and
//! disputes — behind interfaces that do not leak any particular payment
//! provider. Swapping Stripe for PayPal is a configuration change, not a
//! rewrite.
//!
//! # Design principles
//!
//! 1. **No floats, ever.** All amounts are integer minor units
//!    ([`money::Money`]), and every split goes through the largest-remainder
//!    allocator in [`money::allocate`], so parts always sum exactly to the
//!    whole.
//! 2. **Prove it balances.** [`pricing::Quote::verify`],
//!    [`payment::SettlementPlan::verify`], [`payment::TenderPlan::verify`] and
//!    [`payment::RefundPlan::verify`] assert their invariants and are called
//!    automatically. A rounding regression fails loudly instead of quietly
//!    shorting a merchant.
//! 3. **Card data never enters the process.** The API accepts gateway tokens
//!    only; see [`secret`] for the PCI DSS reasoning.
//! 4. **Everything is a trait.** Gateways, tax, FX, fraud and storage are all
//!    pluggable, with a complete working implementation of each included.
//!
//! # Quick start
//!
//! ```
//! use std::sync::Arc;
//! use payments::prelude::*;
//!
//! # async fn run() -> payments::Result<()> {
//! // 1. Wire the engine: pricing + gateways + storage.
//! let pricing = PricingEngine::new(Arc::new(NoTaxCalculator));
//! let gateways = GatewayRegistry::new().register(Arc::new(MockGateway::new()));
//! let repositories = Repositories {
//!     carts: Arc::new(InMemoryCartRepository::new()),
//!     orders: Arc::new(InMemoryOrderRepository::new()),
//!     payments: Arc::new(InMemoryPaymentRepository::new()),
//!     discounts: Arc::new(InMemoryDiscountRepository::new()),
//!     gift_cards: Arc::new(InMemoryGiftCardRepository::new()),
//!     shop_credit: Arc::new(InMemoryShopCreditRepository::new()),
//! };
//! let engine = CheckoutEngine::new(pricing, gateways, repositories.clone());
//!
//! // 2. Build a cart.
//! let carts = CartService::new(repositories.carts.clone());
//! let cart = carts.create(Some(CustomerId::new()), Currency::USD).await?;
//! let item = CartItem::new("shop-1", "tee", "T-shirt", Money::from_minor(2_500, Currency::USD), 2)?;
//! let cart = carts.add_item(&cart.id, item).await?;
//!
//! // 3. Show live totals, then charge.
//! let quote = engine.quote(&cart).await?;
//! assert_eq!(quote.amount_due(), Money::from_minor(5_000, Currency::USD));
//!
//! let request = CheckoutRequest::new(cart.id.clone(), "checkout-1").with_tender(
//!     TenderOffer::gateway(
//!         GatewayId::from_static("mock"),
//!         single_use("tok_visa"),
//!         "visa •••• 4242",
//!     ),
//! );
//! let result = engine.checkout(&request).await?;
//! assert_eq!(result.order.status, OrderStatus::Paid);
//! # Ok(())
//! # }
//! ```
//!
//! # Module map
//!
//! | module | responsibility |
//! |---|---|
//! | [`money`] | exact amounts, currencies, penny-perfect allocation |
//! | [`cart`] | carts, line items, fulfilment grouping |
//! | [`pricing`] | discounts, tax, FX, the quote engine |
//! | [`order`] | the placed order and its state machine |
//! | [`payment`] | tenders, authorisation/capture, splits, refunds, disputes |
//! | [`ledger`] | gift cards and shop credit |
//! | [`gateway`] | the provider abstraction, routing and adapters |
//! | [`checkout`] | the orchestrator that ties it all together |
//! | [`webhook`] | signature verification, deduplication, dispatch |
//! | [`storage`] | repository traits and an in-memory implementation |
//! | [`fraud`] | pre-authorisation screening |
//! | [`reporting`] | sales, per-shop and per-funder analytics |

#![warn(missing_docs, rust_2018_idioms)]

pub mod address;
pub mod cart;
pub mod checkout;
pub mod error;
pub mod fraud;
pub mod gateway;
pub mod ids;
pub mod ledger;
pub mod money;
pub mod order;
pub mod payment;
pub mod pricing;
pub mod reporting;
pub mod secret;
pub mod storage;
pub mod webhook;

pub use error::{DeclineCode, Error, ErrorCategory, Result};

/// Free-form key/value data carried on most entities and forwarded to gateways.
///
/// A `BTreeMap` rather than a `HashMap` so that serialisation is deterministic,
/// which matters for idempotency fingerprints and signature payloads.
pub type Metadata = std::collections::BTreeMap<String, String>;

/// The types most applications need, in one import.
pub mod prelude {
    pub use crate::address::{Address, CountryCode, Jurisdiction};
    pub use crate::cart::{
        Cart, CartItem, CartService, FulfillmentMethod, FulfillmentSelection, FulfillmentStatus,
    };
    pub use crate::checkout::{
        CheckoutConfig, CheckoutEngine, CheckoutRequest, CheckoutResult, Repositories, single_use,
    };
    pub use crate::error::{DeclineCode, Error, Result};
    pub use crate::fraud::{FraudPolicy, RiskDecision, RuleBasedFraudEngine};
    pub use crate::gateway::{
        Capabilities, CaptureMode, GatewayId, GatewayRegistry, InstrumentRef, PaymentGateway,
        RoutingRule,
    };
    pub use crate::ids::{
        AccountId, CartId, CustomerId, GiftCardId, LineItemId, OrderId, PaymentId, ShopId,
    };
    pub use crate::ledger::{GiftCard, ShopCreditAccount};
    pub use crate::money::{Currency, Money, Rounding, allocate};
    pub use crate::order::{Order, OrderStatus};
    pub use crate::payment::{
        Payment, PaymentStatus, PlatformFeePolicy, RefundRequest, SettlementPlan, ShopAccounts,
        TenderOffer, TenderPlan,
    };
    pub use crate::pricing::{
        Discount, DiscountScope, NoTaxCalculator, PricingConfig, PricingEngine,
        RateTableTaxCalculator, TaxCode, TaxMode, TaxRule,
    };
    pub use crate::reporting::{DateRange, Reporting};
    pub use crate::webhook::{Headers, WebhookHandler, WebhookProcessor};

    #[cfg(feature = "mock-gateway")]
    pub use crate::gateway::mock::MockGateway;

    #[cfg(feature = "memory-store")]
    pub use crate::storage::memory::{
        InMemoryCartRepository, InMemoryDiscountRepository, InMemoryDisputeRepository,
        InMemoryGiftCardRepository, InMemoryIdempotencyStore, InMemoryOrderRepository,
        InMemoryPaymentMethodRepository, InMemoryPaymentRepository, InMemoryProcessedEventStore,
        InMemoryShopCreditRepository,
    };
}
