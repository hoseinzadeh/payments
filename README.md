# payments

A gateway-agnostic payments engine for marketplaces, in Rust.

It models the whole commerce money path — cart, pricing, tax, discounts,
checkout, authorisation, capture, split settlement, multi-tender funding,
refunds and disputes — behind interfaces that do not leak any particular
payment provider. Swapping Stripe for PayPal is a configuration change.

```rust
let quote = engine.quote(&cart).await?;          // live totals, no side effects
let result = engine.checkout(&request).await?;   // screen, fund, authorise
engine.confirm(&order_id).await?;                // merchant approves capture
engine.capture_fulfillment_group(&order_id, &group).await?; // capture on delivery
engine.refund(&order_id, &RefundRequest::full("r1")).await?;
```

## Why it is built this way

**Money is never a float.** Every amount is an integer count of minor units
(`Money`), and every split — a discount across lines, a charge across shops, a
refund across instruments — goes through one largest-remainder allocator
(`money::allocate`) whose parts are guaranteed to sum exactly to the whole.
There is a property test that checks this over every total from 0 to 200 against
five weight distributions.

**Plans prove themselves before money moves.** `Quote::verify`,
`SettlementPlan::verify`, `TenderPlan::verify` and `RefundPlan::verify` assert
their invariants and are called automatically by their constructors. The central
one is:

```text
merchant_gross = customer_total + subsidy_total
```

what a shop is owed equals what the shopper pays plus what third parties pay on
their behalf. A rounding regression fails loudly instead of quietly shorting a
merchant by a cent per order.

**Card data cannot enter the process.** No type in the public API accepts a PAN
or CVV; instruments are referenced only by gateway tokens. Secrets that must be
held (API keys, webhook signing secrets) use `SecretString`, which redacts in
`Debug`/`Display`, refuses to serialise, zeroes on drop and compares in constant
time. This keeps an integrating service in PCI DSS **SAQ A**.

**Everything is a trait.** `PaymentGateway`, `TaxCalculator`, `CurrencyConverter`,
`FraudEngine` and the storage repositories are all pluggable, and each ships with
a complete, tested implementation so the crate is useful before you write any.

## What it handles

| Requirement | Where |
|---|---|
| Unified interface over Stripe / PayPal / any gateway | `gateway::PaymentGateway`, `gateway::Capabilities` |
| Per-gateway feature differences, checked up front | `Capabilities::require` |
| Card-on-file (tokens only, never card data) | `payment::method::PaymentMethodRef`, `secret` |
| Authorise now, capture later, partial & multi capture | `payment::Payment`, `gateway::CaptureMode` |
| Merchant confirmation before capture | `Payment::confirm`, enforced by `check_capture` |
| Authorisation expiry | `Payment::is_authorization_expired`, `list_expiring` |
| Connected accounts / split payments | `payment::split::SettlementPlan` |
| Webhooks: verify, deduplicate, dispatch | `webhook::WebhookProcessor`, `checkout::OrderWebhookHandler` |
| Pluggable storage | `storage::*` traits + `storage::memory` |
| Cart management & per-item delivery options | `cart::Cart`, `cart::FulfillmentGroup` |
| Orders & status tracking | `order::Order`, `order::OrderStatus` |
| Tax (exclusive/inclusive, layered, compound, exemptions) | `pricing::tax` |
| Currency conversion | `pricing::fx` |
| Fraud screening before authorisation | `fraud::RuleBasedFraudEngine` |
| Discounts: merchant vs. subsidised, stacking, scoping | `pricing::discount` |
| Live totals during checkout | `CheckoutEngine::quote` |
| Full & partial refunds with split recalculation | `payment::refund::RefundPlan` |
| Shop credit, gift cards, multi-tender | `ledger`, `payment::tender` |
| Disputes & chargebacks | `payment::dispute` |
| Reporting & analytics | `reporting` |

## The subsidy rule

The crate distinguishes *who pays* for a discount, because it changes both the
money movement and the tax base:

* **Merchant-funded** — the shop gives up revenue. It receives less, and the
  taxable base shrinks with it.
* **Subsidised** — a platform, brand or employer pays the difference. The shop
  still receives the full price, the funder is billed, and the taxable base is
  **unchanged**, because the shop's consideration did not change.

Getting this backwards is how marketplaces under-remit tax, so it is explicit in
`Discount::reduces_taxable_base()` and overridable where local rules differ.

## Multi-tender and refunds

A shopper can combine a gift card, shop credit and a card. Stored value is spent
first (no interchange, and worthless to the shopper if the order fails), and
shop credit may only pay for its own shop's share of the basket — enforced, not
documented. Each tender records *what it paid for, per shop*, which is what makes
a partial refund of a multi-shop order exact:

```text
order    bookstore 60.00 + record-shop 40.00 = 100.00
funded   gift card 30.00, shop credit 10.00, card 60.00
refund   one book (30.00) -> gift card 9.00, credit 5.00, card 16.00
```

## Getting started

```toml
[dependencies]
payments = { version = "0.1", features = ["memory-store", "mock-gateway"] }
```

Run the worked examples:

```bash
cargo run --example single_shop           # scenario 1
cargo run --example marketplace_split     # scenarios 2-4, capture on delivery
cargo run --example multi_tender_refund   # scenarios 5-6
```

### Features

| feature | contents |
|---|---|
| `memory-store` *(default)* | in-memory implementation of every repository |
| `mock-gateway` *(default)* | deterministic in-process gateway with real bookkeeping |
| `stripe` | Stripe Payment Intents adapter |
| `paypal` | PayPal Orders v2 adapter |

### Bringing your own HTTP client

The crate depends on no HTTP client. Adapters are written against
`gateway::http::HttpTransport`; implement it over whatever you already use:

```rust
#[async_trait]
impl HttpTransport for ReqwestTransport {
    async fn execute(&self, request: HttpRequest) -> payments::Result<HttpResponse> {
        // ~30 lines mapping HttpRequest -> reqwest -> HttpResponse
    }
}
```

This also makes adapters testable without a network: `MockTransport` records the
exact request an adapter builds and replays canned provider responses, which is
how the Stripe and PayPal adapters are tested here.

## Testing

```bash
cargo test --all-features   # 208 tests
cargo clippy --all-features --all-targets
```

`tests/scenarios.rs` walks every scenario in `requirements.md` end to end with
exact expected amounts, and asserts after each one that the quote, the
settlement and every refund balance.

## Production checklist

The crate is complete and correct in the pure-logic layers; these are the
integration points you own:

* Implement the `storage` traits over a real database. Honour the version check
  in `save` (`WHERE version < $new_version`) or you lose optimistic concurrency.
* Implement `HttpTransport` with timeouts, connection pooling and retries.
* Feed `RiskInputs` from your own order history, and consider your processor's
  risk engine as well as this one.
* Serve webhooks on a route that hands `WebhookProcessor::process` the **raw**
  body — re-serialising JSON invalidates every signature scheme in use.
* Run `Reporting` against a read replica; `list_all` is deliberately simple and
  should be paginated in your implementation.

## License

MIT OR Apache-2.0
