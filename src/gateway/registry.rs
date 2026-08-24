//! Gateway routing.
//!
//! Real deployments run more than one processor: a primary, a fallback for when
//! it has an incident, a local acquirer for a particular currency, and a wallet
//! provider. The registry picks the right adapter for a payment from declarative
//! rules, so routing policy lives in configuration instead of in `if` statements
//! scattered through the checkout code.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::address::CountryCode;
use crate::error::{Error, Result};
use crate::gateway::{GatewayId, PaymentGateway};
use crate::money::Currency;

/// A routing condition. All specified fields must match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingRule {
    /// Restrict to one currency.
    pub currency: Option<Currency>,
    /// Restrict to buyers in one country.
    pub country: Option<CountryCode>,
    /// Require a capability, e.g. `"connected_accounts"`.
    pub requires: Option<&'static str>,
    /// Lower numbers are tried first.
    pub priority: i32,
    /// Gateway to use when the rule matches.
    pub gateway: GatewayId,
}

impl RoutingRule {
    /// A rule that sends everything to one gateway.
    pub fn always(gateway: GatewayId) -> Self {
        Self { currency: None, country: None, requires: None, priority: 0, gateway }
    }

    /// Builder: restrict to a currency.
    pub fn for_currency(mut self, currency: Currency) -> Self {
        self.currency = Some(currency);
        self
    }

    /// Builder: restrict to a buyer country.
    pub fn for_country(mut self, country: CountryCode) -> Self {
        self.country = Some(country);
        self
    }

    /// Builder: require a capability.
    pub fn requiring(mut self, capability: &'static str) -> Self {
        self.requires = Some(capability);
        self
    }

    /// Builder: set the priority.
    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = priority;
        self
    }
}

/// What a payment needs from a gateway.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutingContext {
    /// Currency of the charge.
    pub currency: Option<Currency>,
    /// Buyer's country.
    pub country: Option<CountryCode>,
    /// Capabilities the payment requires.
    pub required_capabilities: Vec<&'static str>,
    /// Force a specific gateway, bypassing the rules.
    pub preferred: Option<GatewayId>,
}

impl RoutingContext {
    /// Route a charge in `currency`.
    pub fn for_currency(currency: Currency) -> Self {
        Self { currency: Some(currency), ..Default::default() }
    }

    /// Builder: require a capability.
    pub fn requiring(mut self, capability: &'static str) -> Self {
        self.required_capabilities.push(capability);
        self
    }

    /// Builder: pin the gateway.
    pub fn preferring(mut self, gateway: GatewayId) -> Self {
        self.preferred = Some(gateway);
        self
    }
}

/// Holds the configured adapters and decides which one handles a payment.
#[derive(Clone, Default)]
pub struct GatewayRegistry {
    gateways: BTreeMap<String, Arc<dyn PaymentGateway>>,
    rules: Vec<RoutingRule>,
    default_gateway: Option<GatewayId>,
}

impl GatewayRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an adapter. The first one registered becomes the default.
    pub fn register(mut self, gateway: Arc<dyn PaymentGateway>) -> Self {
        let id = gateway.id();
        if self.default_gateway.is_none() {
            self.default_gateway = Some(id.clone());
        }
        self.gateways.insert(id.to_string(), gateway);
        self
    }

    /// Add a routing rule.
    pub fn with_rule(mut self, rule: RoutingRule) -> Self {
        self.rules.push(rule);
        self.rules.sort_by_key(|rule| rule.priority);
        self
    }

    /// Set the fallback gateway explicitly.
    pub fn with_default(mut self, gateway: GatewayId) -> Self {
        self.default_gateway = Some(gateway);
        self
    }

    /// Look an adapter up by id.
    pub fn get(&self, id: &GatewayId) -> Result<Arc<dyn PaymentGateway>> {
        self.gateways
            .get(id.as_str())
            .cloned()
            .ok_or_else(|| Error::configuration(format!("no gateway registered as '{id}'")))
    }

    /// Every registered adapter.
    pub fn all(&self) -> impl Iterator<Item = &Arc<dyn PaymentGateway>> {
        self.gateways.values()
    }

    /// Choose the adapter for a payment.
    pub fn route(&self, context: &RoutingContext) -> Result<Arc<dyn PaymentGateway>> {
        if let Some(preferred) = &context.preferred {
            let gateway = self.get(preferred)?;
            self.check(&gateway, context)?;
            return Ok(gateway);
        }

        for rule in &self.rules {
            if let Some(currency) = rule.currency
                && context.currency != Some(currency)
            {
                continue;
            }
            if let Some(country) = rule.country
                && context.country != Some(country)
            {
                continue;
            }
            let Ok(gateway) = self.get(&rule.gateway) else {
                continue;
            };
            if let Some(capability) = rule.requires
                && gateway.capabilities().require(&rule.gateway, capability).is_err()
            {
                continue;
            }
            if self.check(&gateway, context).is_ok() {
                return Ok(gateway);
            }
        }

        if let Some(default) = &self.default_gateway {
            let gateway = self.get(default)?;
            self.check(&gateway, context)?;
            return Ok(gateway);
        }

        Err(Error::configuration("no gateway is configured"))
    }

    fn check(&self, gateway: &Arc<dyn PaymentGateway>, context: &RoutingContext) -> Result<()> {
        let capabilities = gateway.capabilities();
        let id = gateway.id();
        if let Some(currency) = context.currency
            && !capabilities.supports_currency(currency)
        {
            return Err(Error::configuration(format!(
                "gateway '{id}' does not support {currency}"
            )));
        }
        for capability in &context.required_capabilities {
            capabilities.require(&id, capability)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::{
        AuthorizeRequest, AuthorizeResponse, CancelRequest, Capabilities, CaptureRequest,
        CaptureResponse, GatewayRefundRequest, GatewayRefundResponse,
    };
    use async_trait::async_trait;

    struct StubGateway {
        id: GatewayId,
        capabilities: Capabilities,
    }

    #[async_trait]
    impl PaymentGateway for StubGateway {
        fn id(&self) -> GatewayId {
            self.id.clone()
        }
        fn capabilities(&self) -> Capabilities {
            self.capabilities.clone()
        }
        async fn authorize(&self, _r: &AuthorizeRequest) -> Result<AuthorizeResponse> {
            unimplemented!()
        }
        async fn capture(&self, _r: &CaptureRequest) -> Result<CaptureResponse> {
            unimplemented!()
        }
        async fn cancel(&self, _r: &CancelRequest) -> Result<AuthorizeResponse> {
            unimplemented!()
        }
        async fn refund(&self, _r: &GatewayRefundRequest) -> Result<GatewayRefundResponse> {
            unimplemented!()
        }
        async fn fetch_transaction(&self, _id: &str) -> Result<AuthorizeResponse> {
            unimplemented!()
        }
    }

    fn stub(id: &'static str, capabilities: Capabilities) -> Arc<dyn PaymentGateway> {
        Arc::new(StubGateway { id: GatewayId::from_static(id), capabilities })
    }

    fn registry() -> GatewayRegistry {
        let mut eur_only = Capabilities::full();
        eur_only.currencies.insert("EUR".to_owned());
        let basic = Capabilities::default();

        GatewayRegistry::new()
            .register(stub("primary", Capabilities::full()))
            .register(stub("eu", eur_only))
            .register(stub("legacy", basic))
    }

    #[test]
    fn falls_back_to_the_default_gateway() {
        let registry = registry();
        let chosen = registry.route(&RoutingContext::for_currency(Currency::USD)).unwrap();
        assert_eq!(chosen.id().as_str(), "primary");
    }

    #[test]
    fn currency_rules_win_over_the_default() {
        let registry = registry().with_rule(
            RoutingRule::always(GatewayId::from_static("eu")).for_currency(Currency::EUR),
        );
        let chosen = registry.route(&RoutingContext::for_currency(Currency::EUR)).unwrap();
        assert_eq!(chosen.id().as_str(), "eu");

        let chosen = registry.route(&RoutingContext::for_currency(Currency::USD)).unwrap();
        assert_eq!(chosen.id().as_str(), "primary");
    }

    #[test]
    fn rules_that_cannot_satisfy_the_context_are_skipped() {
        let registry = registry().with_rule(
            RoutingRule::always(GatewayId::from_static("legacy")).with_priority(-10),
        );
        // `legacy` cannot do delayed capture, so routing falls through.
        let context = RoutingContext::for_currency(Currency::USD).requiring("delayed_capture");
        let chosen = registry.route(&context).unwrap();
        assert_eq!(chosen.id().as_str(), "primary");
    }

    #[test]
    fn preferred_gateway_is_honoured_but_still_validated() {
        let registry = registry();
        let context = RoutingContext::for_currency(Currency::USD)
            .preferring(GatewayId::from_static("eu"));
        // `eu` only handles EUR, so pinning it for USD is an error rather than
        // a silent reroute.
        assert!(registry.route(&context).is_err());

        let context = RoutingContext::for_currency(Currency::EUR)
            .preferring(GatewayId::from_static("eu"));
        assert_eq!(registry.route(&context).unwrap().id().as_str(), "eu");
    }

    #[test]
    fn unknown_gateways_and_empty_registries_error() {
        let registry = registry();
        assert!(registry.get(&GatewayId::from_static("nope")).is_err());
        assert!(GatewayRegistry::new().route(&RoutingContext::default()).is_err());
        assert_eq!(registry.all().count(), 3);
    }
}
