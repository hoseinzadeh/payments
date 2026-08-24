//! Reporting and analytics.
//!
//! Figures are derived from the orders themselves rather than from a separate
//! counter, so a report can always be recomputed and reconciled against the
//! underlying records. Every report is scoped to a time range and reports in a
//! single currency; mixed-currency estates should run one report per currency
//! or convert with a [`CurrencyConverter`](crate::pricing::CurrencyConverter).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::Result;
use crate::ids::ShopId;
use crate::money::{Currency, Money};
use crate::order::{Order, OrderStatus};
use crate::storage::OrderRepository;

/// An inclusive-exclusive time window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateRange {
    /// Start, inclusive.
    pub from: DateTime<Utc>,
    /// End, exclusive.
    pub to: DateTime<Utc>,
}

impl DateRange {
    /// A range covering everything.
    pub fn all_time() -> Self {
        Self { from: DateTime::<Utc>::MIN_UTC, to: DateTime::<Utc>::MAX_UTC }
    }

    /// A custom range.
    pub fn new(from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        Self { from, to }
    }

    /// Whether an instant falls inside the range.
    pub fn contains(&self, at: DateTime<Utc>) -> bool {
        at >= self.from && at < self.to
    }
}

/// Headline figures for a period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SalesReport {
    /// Currency of every amount.
    pub currency: Currency,
    /// Period covered.
    pub range: DateRange,
    /// Number of orders that collected money.
    pub order_count: u64,
    /// Total the shoppers were charged.
    pub gross_volume: Money,
    /// Total returned to shoppers.
    pub refunded: Money,
    /// `gross_volume - refunded`.
    pub net_volume: Money,
    /// Tax collected on behalf of shops.
    pub tax_collected: Money,
    /// Commission the platform earned.
    pub platform_fees: Money,
    /// Amount paid by third-party funders.
    pub subsidies: Money,
    /// Discounts absorbed by shops.
    pub merchant_discounts: Money,
    /// Shipping charged.
    pub shipping: Money,
    /// Orders by status.
    pub by_status: BTreeMap<String, u64>,
}

impl SalesReport {
    /// Mean value of the orders in the period.
    pub fn average_order_value(&self) -> Money {
        if self.order_count == 0 {
            return Money::zero(self.currency);
        }
        self.gross_volume
            .mul_ratio(1, self.order_count as i64, crate::money::Rounding::HalfEven)
            .unwrap_or_else(|_| Money::zero(self.currency))
    }

    /// Refunds as a share of gross volume, in basis points.
    pub fn refund_rate_basis_points(&self) -> i64 {
        self.refunded.ratio_basis_points(self.gross_volume).unwrap_or(0)
    }
}

/// Per-shop figures for a period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShopReport {
    /// The shop.
    pub shop_id: ShopId,
    /// Orders containing this shop's items.
    pub order_count: u64,
    /// Everything the shop was owed, tax included.
    pub gross: Money,
    /// Commission deducted.
    pub platform_fees: Money,
    /// Amount clawed back by refunds.
    pub refunded: Money,
    /// `gross - platform_fees - refunded`.
    pub net: Money,
    /// Tax the shop must remit.
    pub tax: Money,
    /// Part of `gross` paid by third-party funders.
    pub subsidised: Money,
}

/// What each funder owes for a period.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunderReport {
    /// The funding account.
    pub funder: crate::ids::AccountId,
    /// Amount funded.
    pub funded: Money,
    /// Amount reclaimed through refunds.
    pub reclaimed: Money,
    /// `funded - reclaimed`.
    pub net: Money,
}

/// Computes reports from the order store.
#[derive(Clone)]
pub struct Reporting {
    orders: Arc<dyn OrderRepository>,
}

impl Reporting {
    /// Build a reporting service.
    pub fn new(orders: Arc<dyn OrderRepository>) -> Self {
        Self { orders }
    }

    /// Headline sales figures.
    pub async fn sales(&self, currency: Currency, range: DateRange) -> Result<SalesReport> {
        let orders = self.orders_in(currency, range).await?;
        let zero = Money::zero(currency);

        let mut report = SalesReport {
            currency,
            range,
            order_count: 0,
            gross_volume: zero,
            refunded: zero,
            net_volume: zero,
            tax_collected: zero,
            platform_fees: zero,
            subsidies: zero,
            merchant_discounts: zero,
            shipping: zero,
            by_status: BTreeMap::new(),
        };

        for order in &orders {
            *report.by_status.entry(status_label(order.status)).or_insert(0) += 1;
            if !order.status.has_funds() && order.status != OrderStatus::Refunded {
                continue;
            }
            report.order_count += 1;
            report.gross_volume = report.gross_volume.try_add(order.amount_captured)?;
            report.refunded = report.refunded.try_add(order.amount_refunded)?;
            report.tax_collected = report.tax_collected.try_add(order.quote.totals.tax)?;
            report.platform_fees =
                report.platform_fees.try_add(order.settlement.platform_fee_total)?;
            report.subsidies =
                report.subsidies.try_add(order.quote.totals.subsidy_discount)?;
            report.merchant_discounts =
                report.merchant_discounts.try_add(order.quote.totals.merchant_discount)?;
            report.shipping = report.shipping.try_add(order.quote.totals.shipping)?;
        }
        report.net_volume = report.gross_volume.try_sub(report.refunded)?;
        Ok(report)
    }

    /// Per-shop settlement figures, sorted by net descending.
    pub async fn by_shop(&self, currency: Currency, range: DateRange) -> Result<Vec<ShopReport>> {
        let orders = self.orders_in(currency, range).await?;
        let zero = Money::zero(currency);
        let mut reports: BTreeMap<String, ShopReport> = BTreeMap::new();

        for order in &orders {
            if !order.status.has_funds() && order.status != OrderStatus::Refunded {
                continue;
            }
            for settlement in &order.settlement.shops {
                let entry =
                    reports.entry(settlement.shop_id.to_string()).or_insert_with(|| ShopReport {
                        shop_id: settlement.shop_id.clone(),
                        order_count: 0,
                        gross: zero,
                        platform_fees: zero,
                        refunded: zero,
                        net: zero,
                        tax: zero,
                        subsidised: zero,
                    });
                entry.order_count += 1;
                entry.gross = entry.gross.try_add(settlement.gross)?;
                entry.platform_fees = entry.platform_fees.try_add(settlement.platform_fee)?;
                entry.tax = entry.tax.try_add(settlement.tax)?;
                entry.subsidised = entry.subsidised.try_add(settlement.funded_by_subsidy)?;

                for record in &order.refunds {
                    for refund in &record.plan.shops {
                        if refund.shop_id == settlement.shop_id {
                            entry.refunded = entry.refunded.try_add(refund.gross)?;
                            entry.platform_fees =
                                entry.platform_fees.try_sub(refund.platform_fee_returned)?;
                        }
                    }
                }
            }
        }

        let mut reports: Vec<ShopReport> = reports
            .into_values()
            .map(|mut report| -> Result<ShopReport> {
                report.net = report
                    .gross
                    .try_sub(report.platform_fees)?
                    .try_sub(report.refunded)?;
                Ok(report)
            })
            .collect::<Result<Vec<_>>>()?;
        reports.sort_by_key(|report| std::cmp::Reverse(report.net.minor()));
        Ok(reports)
    }

    /// What each third-party funder owes, net of reclaims.
    pub async fn by_funder(&self, currency: Currency, range: DateRange) -> Result<Vec<FunderReport>> {
        let orders = self.orders_in(currency, range).await?;
        let zero = Money::zero(currency);
        let mut reports: BTreeMap<String, FunderReport> = BTreeMap::new();

        for order in &orders {
            for funder in &order.settlement.funders {
                let entry =
                    reports.entry(funder.funder.to_string()).or_insert_with(|| FunderReport {
                        funder: funder.funder.clone(),
                        funded: zero,
                        reclaimed: zero,
                        net: zero,
                    });
                entry.funded = entry.funded.try_add(funder.amount)?;
            }
            for record in &order.refunds {
                for refund in &record.plan.funders {
                    let entry =
                        reports.entry(refund.funder.to_string()).or_insert_with(|| FunderReport {
                            funder: refund.funder.clone(),
                            funded: zero,
                            reclaimed: zero,
                            net: zero,
                        });
                    entry.reclaimed = entry.reclaimed.try_add(refund.amount)?;
                }
            }
        }

        reports
            .into_values()
            .map(|mut report| -> Result<FunderReport> {
                report.net = report.funded.try_sub(report.reclaimed)?;
                Ok(report)
            })
            .collect()
    }

    async fn orders_in(&self, currency: Currency, range: DateRange) -> Result<Vec<Order>> {
        Ok(self
            .orders
            .list_all()
            .await?
            .into_iter()
            .filter(|order| order.currency == currency && range.contains(order.created_at))
            .collect())
    }
}

fn status_label(status: OrderStatus) -> String {
    format!("{status:?}").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::address::{Address, CountryCode};
    use crate::cart::{Cart, CartItem};
    use crate::gateway::{GatewayId, InstrumentRef};
    use crate::ids::AccountId;
    use crate::order::Order;
    use crate::payment::refund::{RefundPlan, RefundRecord, RefundRequest};
    use crate::payment::split::{PlatformFeePolicy, SettlementPlan, ShopAccounts};
    use crate::payment::tender::{TenderOffer, TenderPlan};
    use crate::pricing::{Discount, NoTaxCalculator, PricingEngine};
    use crate::storage::memory::InMemoryOrderRepository;

    fn usd(minor: i64) -> Money {
        Money::from_minor(minor, Currency::USD)
    }

    async fn place(items: &[(&str, i64)], discounts: Vec<Discount>) -> Order {
        let engine = PricingEngine::new(Arc::new(NoTaxCalculator));
        let mut cart = Cart::new(Currency::USD);
        cart.set_shipping_address(Address::new(CountryCode::US));
        for (shop, price) in items {
            cart.add_item(CartItem::new(*shop, "sku", "Item", usd(*price), 1).unwrap()).unwrap();
        }
        let quote = engine.quote(&cart, &discounts).await.unwrap();
        let settlement = SettlementPlan::from_quote(
            &quote,
            &ShopAccounts::new(),
            &PlatformFeePolicy::percentage(1_000),
        )
        .unwrap();
        let tenders = TenderPlan::build(
            &quote,
            &[TenderOffer::gateway(
                GatewayId::from_static("mock"),
                InstrumentRef::SingleUseToken { token: "tok".into() },
                "card",
            )],
        )
        .unwrap();
        let mut order = Order::from_quote(&cart, quote, settlement, &tenders).unwrap();
        order.transition_to(OrderStatus::Authorized).unwrap();
        let total = order.total();
        order.record_capture(total).unwrap();
        order
    }

    #[tokio::test]
    async fn sales_report_aggregates_captures_and_refunds() {
        let repository = Arc::new(InMemoryOrderRepository::new());
        let first = place(&[("shop-1", 10_000)], vec![]).await;
        repository.save(&first).await.unwrap();

        let mut second = place(&[("shop-1", 6_000), ("shop-2", 4_000)], vec![]).await;
        let plan = RefundPlan::build(&second, &RefundRequest::amount("r1", usd(2_000))).unwrap();
        second
            .record_refund(RefundRecord::new(plan, &RefundRequest::amount("r1", usd(2_000))))
            .unwrap();
        repository.save(&second).await.unwrap();

        let reporting = Reporting::new(repository);
        let report = reporting.sales(Currency::USD, DateRange::all_time()).await.unwrap();

        assert_eq!(report.order_count, 2);
        assert_eq!(report.gross_volume, usd(20_000));
        assert_eq!(report.refunded, usd(2_000));
        assert_eq!(report.net_volume, usd(18_000));
        // 10 % of 100.00 in each order.
        assert_eq!(report.platform_fees, usd(2_000));
        assert_eq!(report.average_order_value(), usd(10_000));
        assert_eq!(report.refund_rate_basis_points(), 1_000);
    }

    #[tokio::test]
    async fn shop_report_nets_off_fees_and_refunds() {
        let repository = Arc::new(InMemoryOrderRepository::new());
        let order = place(&[("shop-1", 10_000), ("shop-2", 5_000)], vec![]).await;
        repository.save(&order).await.unwrap();

        let reporting = Reporting::new(repository);
        let reports = reporting.by_shop(Currency::USD, DateRange::all_time()).await.unwrap();

        assert_eq!(reports.len(), 2);
        assert_eq!(reports[0].shop_id.as_str(), "shop-1");
        assert_eq!(reports[0].gross, usd(10_000));
        assert_eq!(reports[0].platform_fees, usd(1_000));
        assert_eq!(reports[0].net, usd(9_000));
    }

    #[tokio::test]
    async fn funder_report_tracks_subsidies_and_reclaims() {
        let repository = Arc::new(InMemoryOrderRepository::new());
        let subsidy = Discount::amount_off("SUB", "Benefit", usd(2_000))
            .funded_by(AccountId::from_string("acct_funder"), "benefit");
        let mut order = place(&[("shop-1", 10_000)], vec![subsidy]).await;

        let plan = RefundPlan::build(&order, &RefundRequest::full("r1")).unwrap();
        order.record_refund(RefundRecord::new(plan, &RefundRequest::full("r1"))).unwrap();
        repository.save(&order).await.unwrap();

        let reporting = Reporting::new(repository);
        let reports = reporting.by_funder(Currency::USD, DateRange::all_time()).await.unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].funded, usd(2_000));
        assert_eq!(reports[0].reclaimed, usd(2_000));
        assert_eq!(reports[0].net, usd(0));
    }

    #[tokio::test]
    async fn ranges_and_currencies_filter_the_input() {
        let repository = Arc::new(InMemoryOrderRepository::new());
        repository.save(&place(&[("shop-1", 1_000)], vec![]).await).await.unwrap();
        let reporting = Reporting::new(repository);

        let past = DateRange::new(
            Utc::now() - chrono::Duration::days(30),
            Utc::now() - chrono::Duration::days(1),
        );
        assert_eq!(reporting.sales(Currency::USD, past).await.unwrap().order_count, 0);
        assert_eq!(
            reporting.sales(Currency::EUR, DateRange::all_time()).await.unwrap().order_count,
            0
        );
    }
}
