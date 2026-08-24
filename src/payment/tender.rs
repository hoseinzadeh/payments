//! Multi-tender payments: paying one order with several instruments.
//!
//! A shopper may combine a gift card, shop credit and a card in a single
//! checkout. Two rules make this safe:
//!
//! * **Stored value is spent first.** Gift cards and credit are cheapest for
//!   the merchant (no interchange) and worthless to the shopper if the order
//!   fails, so they are applied before card networks.
//! * **Every tender records what it paid for, per shop.** Without that,
//!   partially refunding a three-shop order paid with a gift card plus a card
//!   is guesswork. [`PlannedTender::shop_allocation`] keeps the mapping exact.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::gateway::{GatewayId, InstrumentRef};
use crate::ids::{GiftCardId, PaymentMethodId, ShopId};
use crate::money::{Currency, Money, allocate_by_weights};
use crate::pricing::Quote;

/// The kind of instrument behind one tender.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TenderKind {
    /// A gift card balance.
    GiftCard {
        /// The card.
        gift_card_id: GiftCardId,
        /// Masked code, for receipts.
        masked_code: String,
    },
    /// Shop credit, spendable only at one shop.
    ShopCredit {
        /// The shop whose credit this is.
        shop_id: ShopId,
    },
    /// A charge through a payment gateway.
    Gateway {
        /// Which gateway.
        gateway: GatewayId,
        /// The vaulted instrument, when the shopper picked a saved card.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        payment_method_id: Option<PaymentMethodId>,
        /// Token to charge.
        instrument: InstrumentRef,
        /// Display label, e.g. `"visa •••• 4242"`.
        label: String,
    },
}

impl TenderKind {
    /// Whether this tender draws on a stored-value balance we control.
    pub fn is_stored_value(&self) -> bool {
        matches!(self, TenderKind::GiftCard { .. } | TenderKind::ShopCredit { .. })
    }

    /// Application order: stored value first, gateways last.
    pub fn default_priority(&self) -> u8 {
        match self {
            TenderKind::GiftCard { .. } => 0,
            TenderKind::ShopCredit { .. } => 1,
            TenderKind::Gateway { .. } => 2,
        }
    }

    /// The shop this tender is restricted to, if any.
    pub fn restricted_shop(&self) -> Option<&ShopId> {
        match self {
            TenderKind::ShopCredit { shop_id } => Some(shop_id),
            _ => None,
        }
    }

    /// A label for receipts.
    pub fn label(&self) -> String {
        match self {
            TenderKind::GiftCard { masked_code, .. } => format!("Gift card {masked_code}"),
            TenderKind::ShopCredit { shop_id } => format!("Store credit ({shop_id})"),
            TenderKind::Gateway { label, .. } => label.clone(),
        }
    }
}

/// An instrument the shopper is offering, with whatever balance it has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenderOffer {
    /// The instrument.
    pub kind: TenderKind,
    /// Spendable balance. `None` means "no limit", which only makes sense for
    /// gateway tenders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<Money>,
    /// Restrict this tender to one shop's share of the order. Gift cards issued
    /// by a single shop set this; shop credit sets it implicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restricted_to_shop: Option<ShopId>,
    /// Hard cap the shopper asked for ("put $20 on this card").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_amount: Option<Money>,
    /// Lower applies first; defaults to [`TenderKind::default_priority`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<u8>,
}

impl TenderOffer {
    /// A gateway tender with no balance limit.
    pub fn gateway(
        gateway: GatewayId,
        instrument: InstrumentRef,
        label: impl Into<String>,
    ) -> Self {
        Self {
            kind: TenderKind::Gateway {
                gateway,
                payment_method_id: None,
                instrument,
                label: label.into(),
            },
            available: None,
            restricted_to_shop: None,
            max_amount: None,
            priority: None,
        }
    }

    /// A gift card tender with a known balance.
    pub fn gift_card(
        gift_card_id: GiftCardId,
        masked_code: impl Into<String>,
        available: Money,
    ) -> Self {
        Self {
            kind: TenderKind::GiftCard { gift_card_id, masked_code: masked_code.into() },
            available: Some(available),
            restricted_to_shop: None,
            max_amount: None,
            priority: None,
        }
    }

    /// A shop-credit tender.
    pub fn shop_credit(shop_id: ShopId, available: Money) -> Self {
        Self {
            kind: TenderKind::ShopCredit { shop_id: shop_id.clone() },
            available: Some(available),
            restricted_to_shop: Some(shop_id),
            max_amount: None,
            priority: None,
        }
    }

    /// Builder: link a saved payment method.
    pub fn with_payment_method(mut self, id: PaymentMethodId) -> Self {
        if let TenderKind::Gateway { payment_method_id, .. } = &mut self.kind {
            *payment_method_id = Some(id);
        }
        self
    }

    /// Builder: restrict a gift card to one shop.
    pub fn restricted_to(mut self, shop_id: ShopId) -> Self {
        self.restricted_to_shop = Some(shop_id);
        self
    }

    /// Builder: cap how much may be taken from this instrument.
    pub fn capped_at(mut self, max: Money) -> Self {
        self.max_amount = Some(max);
        self
    }

    fn effective_priority(&self) -> u8 {
        self.priority.unwrap_or_else(|| self.kind.default_priority())
    }

    fn shop_restriction(&self) -> Option<&ShopId> {
        self.restricted_to_shop.as_ref().or_else(|| self.kind.restricted_shop())
    }
}

/// One instrument with the amount it will be charged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlannedTender {
    /// The instrument.
    pub kind: TenderKind,
    /// Amount to take from it.
    pub amount: Money,
    /// How that amount is attributed to each shop, keyed by shop id. This is
    /// what makes partial refunds reversible against the right instrument.
    pub shop_allocation: BTreeMap<String, Money>,
}

impl PlannedTender {
    /// Amount attributed to one shop.
    pub fn amount_for_shop(&self, shop_id: &ShopId, currency: Currency) -> Money {
        self.shop_allocation
            .get(shop_id.as_str())
            .copied()
            .unwrap_or_else(|| Money::zero(currency))
    }
}

/// The complete funding plan for an order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenderPlan {
    /// Currency of every amount.
    pub currency: Currency,
    /// Amount that must be collected in total.
    pub total_due: Money,
    /// Instruments and their amounts, in application order.
    pub tenders: Vec<PlannedTender>,
}

impl TenderPlan {
    /// Build a plan covering `quote` from the offered instruments.
    ///
    /// Fails with [`Error::InsufficientFunds`] if the offers cannot cover the
    /// total — better to know before charging the gift card than after.
    pub fn build(quote: &Quote, offers: &[TenderOffer]) -> Result<Self> {
        let currency = quote.currency;
        let total_due = quote.totals.total;

        // Remaining amount owed per shop, in quote order.
        let mut shop_ids: Vec<ShopId> = Vec::new();
        let mut remaining: Vec<Money> = Vec::new();
        for totals in &quote.shop_totals {
            shop_ids.push(totals.shop_id.clone());
            remaining.push(totals.customer_total);
        }

        let mut ordered: Vec<&TenderOffer> = offers.iter().collect();
        ordered.sort_by_key(|offer| offer.effective_priority());

        let mut tenders = Vec::new();
        for offer in ordered {
            let outstanding = Money::sum(remaining.iter().copied(), currency)?;
            if !outstanding.is_positive() {
                break;
            }

            // How much of the order this instrument is allowed to touch.
            let addressable = match offer.shop_restriction() {
                Some(shop) => match shop_ids.iter().position(|candidate| candidate == shop) {
                    Some(index) => remaining[index],
                    None => Money::zero(currency),
                },
                None => outstanding,
            };
            if !addressable.is_positive() {
                continue;
            }

            let mut amount = addressable;
            if let Some(available) = offer.available {
                available.assert_same_currency(amount)?;
                amount = amount.try_min(available)?;
            }
            if let Some(max) = offer.max_amount {
                max.assert_same_currency(amount)?;
                amount = amount.try_min(max)?;
            }
            if !amount.is_positive() {
                continue;
            }

            // Attribute the amount to shops and deduct it.
            let mut shop_allocation = BTreeMap::new();
            match offer.shop_restriction() {
                Some(shop) => {
                    let index = shop_ids
                        .iter()
                        .position(|candidate| candidate == shop)
                        .expect("addressable was positive");
                    remaining[index] = remaining[index].try_sub(amount)?;
                    shop_allocation.insert(shop.to_string(), amount);
                }
                None => {
                    let shares = allocate_by_weights(amount, &remaining)?;
                    for (index, share) in shares.iter().enumerate() {
                        if share.is_zero() {
                            continue;
                        }
                        remaining[index] = remaining[index].try_sub(*share)?;
                        shop_allocation.insert(shop_ids[index].to_string(), *share);
                    }
                }
            }

            tenders.push(PlannedTender { kind: offer.kind.clone(), amount, shop_allocation });
        }

        let outstanding = Money::sum(remaining.iter().copied(), currency)?;
        if outstanding.is_positive() {
            let covered = total_due.try_sub(outstanding)?;
            return Err(Error::InsufficientFunds {
                source_name: "the selected payment methods".to_owned(),
                available: covered,
                required: total_due,
            });
        }

        let plan = TenderPlan { currency, total_due, tenders };
        plan.verify()?;
        Ok(plan)
    }

    /// The gateway tenders, which are the ones that need an authorisation.
    pub fn gateway_tenders(&self) -> impl Iterator<Item = &PlannedTender> {
        self.tenders.iter().filter(|tender| !tender.kind.is_stored_value())
    }

    /// The stored-value tenders, which are settled internally.
    pub fn stored_value_tenders(&self) -> impl Iterator<Item = &PlannedTender> {
        self.tenders.iter().filter(|tender| tender.kind.is_stored_value())
    }

    /// Total that will be charged to gateways.
    pub fn gateway_total(&self) -> Result<Money> {
        Money::sum(self.gateway_tenders().map(|tender| tender.amount), self.currency)
    }

    /// Assert the plan adds up.
    pub fn verify(&self) -> Result<()> {
        let total = Money::sum(self.tenders.iter().map(|tender| tender.amount), self.currency)?;
        if total != self.total_due {
            return Err(Error::internal(format!(
                "tender plan collects {total} but {} is due",
                self.total_due
            )));
        }
        for tender in &self.tenders {
            let allocated =
                Money::sum(tender.shop_allocation.values().copied(), self.currency)?;
            if allocated != tender.amount {
                return Err(Error::internal(format!(
                    "tender '{}' allocates {allocated} of its {} charge",
                    tender.kind.label(),
                    tender.amount
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Address, CountryCode};
    use crate::cart::{Cart, CartItem};
    use crate::pricing::{NoTaxCalculator, PricingEngine};
    use std::sync::Arc;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    async fn quote_for(items: &[(&str, i64)]) -> Quote {
        let engine = PricingEngine::new(Arc::new(NoTaxCalculator));
        let mut cart = Cart::new(Currency::USD);
        cart.set_shipping_address(Address::new(CountryCode::US));
        for (shop, price) in items {
            cart.add_item(CartItem::new(*shop, "sku", "Item", usd(*price), 1).unwrap()).unwrap();
        }
        engine.quote(&cart, &[]).await.unwrap()
    }

    fn card_offer() -> TenderOffer {
        TenderOffer::gateway(
            GatewayId::from_static("mock"),
            InstrumentRef::SingleUseToken { token: "tok_visa".into() },
            "visa •••• 4242",
        )
    }

    #[tokio::test]
    async fn a_single_card_covers_everything() {
        let quote = quote_for(&[("shop-1", 10_000)]).await;
        let plan = TenderPlan::build(&quote, &[card_offer()]).unwrap();
        assert_eq!(plan.tenders.len(), 1);
        assert_eq!(plan.tenders[0].amount, usd(10_000));
        assert_eq!(plan.gateway_total().unwrap(), usd(10_000));
    }

    #[tokio::test]
    async fn stored_value_is_spent_before_the_card() {
        let quote = quote_for(&[("shop-1", 10_000)]).await;
        let gift = TenderOffer::gift_card(GiftCardId::from_string("gc_1"), "****1234", usd(3_000));
        let plan = TenderPlan::build(&quote, &[card_offer(), gift]).unwrap();

        assert_eq!(plan.tenders.len(), 2);
        assert!(plan.tenders[0].kind.is_stored_value());
        assert_eq!(plan.tenders[0].amount, usd(3_000));
        assert_eq!(plan.tenders[1].amount, usd(7_000));
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn shop_credit_only_pays_for_its_own_shop() {
        let quote = quote_for(&[("shop-1", 6_000), ("shop-2", 4_000)]).await;
        let credit = TenderOffer::shop_credit(ShopId::from_string("shop-2"), usd(9_999));
        let plan = TenderPlan::build(&quote, &[card_offer(), credit]).unwrap();

        let credit_tender = plan.stored_value_tenders().next().unwrap();
        // Capped at shop-2's share, even though the balance would cover more.
        assert_eq!(credit_tender.amount, usd(4_000));
        assert_eq!(
            credit_tender.amount_for_shop(&ShopId::from_string("shop-2"), Currency::USD),
            usd(4_000)
        );
        assert_eq!(
            credit_tender.amount_for_shop(&ShopId::from_string("shop-1"), Currency::USD),
            usd(0)
        );
        assert_eq!(plan.gateway_total().unwrap(), usd(6_000));
    }

    #[tokio::test]
    async fn credit_for_a_shop_that_is_not_in_the_cart_is_skipped() {
        let quote = quote_for(&[("shop-1", 5_000)]).await;
        let credit = TenderOffer::shop_credit(ShopId::from_string("shop-9"), usd(5_000));
        let plan = TenderPlan::build(&quote, &[card_offer(), credit]).unwrap();
        assert_eq!(plan.tenders.len(), 1);
        assert_eq!(plan.gateway_total().unwrap(), usd(5_000));
    }

    #[tokio::test]
    async fn unrestricted_tenders_are_prorated_across_shops() {
        let quote = quote_for(&[("shop-1", 3_333), ("shop-2", 3_333), ("shop-3", 3_334)]).await;
        let gift = TenderOffer::gift_card(GiftCardId::from_string("gc_1"), "****1234", usd(1_000));
        let plan = TenderPlan::build(&quote, &[card_offer(), gift]).unwrap();

        let gift_tender = &plan.tenders[0];
        let allocated =
            Money::sum(gift_tender.shop_allocation.values().copied(), Currency::USD).unwrap();
        assert_eq!(allocated, usd(1_000));
        assert_eq!(gift_tender.shop_allocation.len(), 3);
        plan.verify().unwrap();
    }

    #[tokio::test]
    async fn insufficient_offers_fail_before_anything_is_charged() {
        let quote = quote_for(&[("shop-1", 10_000)]).await;
        let gift = TenderOffer::gift_card(GiftCardId::from_string("gc_1"), "****1234", usd(3_000));
        let error = TenderPlan::build(&quote, &[gift]).unwrap_err();
        match error {
            Error::InsufficientFunds { available, required, .. } => {
                assert_eq!(available, usd(3_000));
                assert_eq!(required, usd(10_000));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn shopper_caps_are_respected() {
        let quote = quote_for(&[("shop-1", 10_000)]).await;
        let capped = card_offer().capped_at(usd(4_000));
        let second = TenderOffer::gateway(
            GatewayId::from_static("mock"),
            InstrumentRef::SingleUseToken { token: "tok_mc".into() },
            "mastercard •••• 5555",
        );
        let plan = TenderPlan::build(&quote, &[capped, second]).unwrap();
        assert_eq!(plan.tenders.len(), 2);
        assert_eq!(plan.tenders[0].amount, usd(4_000));
        assert_eq!(plan.tenders[1].amount, usd(6_000));
    }

    #[tokio::test]
    async fn a_fully_gift_carded_order_needs_no_gateway() {
        let quote = quote_for(&[("shop-1", 2_500)]).await;
        let gift = TenderOffer::gift_card(GiftCardId::from_string("gc_1"), "****1234", usd(5_000));
        let plan = TenderPlan::build(&quote, &[gift, card_offer()]).unwrap();
        assert_eq!(plan.tenders.len(), 1);
        assert_eq!(plan.gateway_total().unwrap(), usd(0));
    }
}
