use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{
    ApiCostAmount, ApiEquivalentCost, ApiModelCost, ApiPricingMetadata, PicoUsd, TokenUsage,
    UsageCall,
};

pub const API_PRICING_CATALOG_REVISION: u32 = 1;
pub const API_PRICING_RATES_AS_OF: &str = "2026-08-27";
pub const API_PRICING_SOURCE_URL: &str = "https://developers.openai.com/api/docs/pricing";

const LONG_CONTEXT_INPUT_THRESHOLD: u64 = 272_000;
const PICO_USD_PER_USD: u128 = 1_000_000_000_000;

const MODEL_UNKNOWN: &str = "api_price_model_unknown";
const SERVICE_TIER_UNKNOWN: &str = "api_price_service_tier_unknown";
const SERVICE_TIER_UNAVAILABLE: &str = "api_price_service_tier_unavailable";
const TOKEN_BREAKDOWN_MISSING: &str = "api_price_token_breakdown_missing";
const TOKEN_BREAKDOWN_INCONSISTENT: &str = "api_price_token_breakdown_inconsistent";
const CACHE_WRITE_RATE_UNAVAILABLE: &str = "api_price_cache_write_rate_unavailable";
const LONG_CONTEXT_UNAVAILABLE: &str = "api_price_long_context_unavailable";
const LONG_CONTEXT_AMBIGUOUS: &str = "api_price_long_context_ambiguous";

/// Prices in micro-US-dollars per one million tokens. Multiplying this rate by
/// a token count yields pico-US-dollars exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TokenRates {
    input: u128,
    cached_input: u128,
    cache_write: Option<u128>,
    output: u128,
}

impl TokenRates {
    const fn new(input: u128, cached_input: u128, cache_write: Option<u128>, output: u128) -> Self {
        Self {
            input,
            cached_input,
            cache_write,
            output,
        }
    }
}

#[derive(Clone, Copy)]
enum LongContextRates {
    Published(TokenRates),
    /// This model has one published price across its full context window.
    Flat,
    /// The relevant public table has no usable long-context row.
    Unavailable,
}

#[derive(Clone, Copy)]
struct TierRates {
    short: TokenRates,
    long: LongContextRates,
}

#[derive(Clone, Copy)]
struct ModelRates {
    standard: TierRates,
    fast: Option<TierRates>,
}

#[derive(Clone, Copy)]
enum ServiceTier {
    Standard,
    Fast,
}

#[derive(Clone, Copy)]
struct CallCost {
    minimum_pico_usd: u128,
    maximum_pico_usd: u128,
    observed_tokens: u64,
    priced: bool,
    partial_reason: Option<&'static str>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ApiCostAccumulator {
    minimum_pico_usd: u128,
    maximum_pico_usd: u128,
    observed_samples: u64,
    priced_samples: u64,
    observed_tokens: u64,
    priced_tokens: u64,
    partial_reasons: BTreeSet<String>,
}

impl ApiCostAccumulator {
    fn add(&mut self, cost: CallCost) {
        self.observed_samples = self.observed_samples.saturating_add(1);
        self.observed_tokens = self.observed_tokens.saturating_add(cost.observed_tokens);
        if cost.priced {
            self.priced_samples = self.priced_samples.saturating_add(1);
            self.priced_tokens = self.priced_tokens.saturating_add(cost.observed_tokens);
            self.minimum_pico_usd = self.minimum_pico_usd.saturating_add(cost.minimum_pico_usd);
            self.maximum_pico_usd = self.maximum_pico_usd.saturating_add(cost.maximum_pico_usd);
        }
        if let Some(reason) = cost.partial_reason {
            self.partial_reasons.insert(reason.to_string());
        }
    }

    pub(crate) fn summary(&self) -> ApiEquivalentCost {
        ApiEquivalentCost {
            amount: self.amount(),
            partial_reasons: self.partial_reasons.iter().cloned().collect(),
            model_breakdown: Vec::new(),
        }
    }

    fn amount(&self) -> ApiCostAmount {
        ApiCostAmount {
            minimum_pico_usd: PicoUsd::new(self.minimum_pico_usd),
            maximum_pico_usd: PicoUsd::new(self.maximum_pico_usd),
            observed_samples: self.observed_samples,
            priced_samples: self.priced_samples,
            observed_tokens: self.observed_tokens,
            priced_tokens: self.priced_tokens,
        }
    }
}

#[derive(Default)]
pub(crate) struct ApiCostAggregation {
    total: ApiCostAccumulator,
    threads: BTreeMap<String, ApiCostAccumulator>,
    turns: BTreeMap<(String, String), ApiCostAccumulator>,
    models: BTreeMap<String, ApiCostAccumulator>,
}

impl ApiCostAggregation {
    pub(crate) fn add_call(&mut self, call: &UsageCall) {
        let cost = price_call(call);
        self.total.add(cost);
        self.threads
            .entry(call.thread_id.clone())
            .or_default()
            .add(cost);
        if let Some(turn_id) = &call.turn_id {
            self.turns
                .entry((call.thread_id.clone(), turn_id.clone()))
                .or_default()
                .add(cost);
        }
        self.models
            .entry(model_name(call).to_string())
            .or_default()
            .add(cost);
    }

    pub(crate) fn total(&self) -> ApiEquivalentCost {
        let mut total = self.total.summary();
        total.model_breakdown = self
            .models
            .iter()
            .map(|(model, cost)| ApiModelCost {
                model: model.clone(),
                amount: cost.amount(),
            })
            .collect();
        total
    }

    pub(crate) fn thread(&self, thread_id: &str) -> ApiCostAmount {
        self.threads
            .get(thread_id)
            .map(ApiCostAccumulator::amount)
            .unwrap_or_default()
    }

    pub(crate) fn turn(&self, thread_id: &str, turn_id: &str) -> ApiCostAmount {
        self.turns
            .get(&(thread_id.to_string(), turn_id.to_string()))
            .map(ApiCostAccumulator::amount)
            .unwrap_or_default()
    }

    pub(crate) fn model(&self, model: &str) -> ApiCostAmount {
        self.models
            .get(model)
            .map(ApiCostAccumulator::amount)
            .unwrap_or_default()
    }
}

pub fn pricing_metadata() -> ApiPricingMetadata {
    ApiPricingMetadata {
        catalog_revision: API_PRICING_CATALOG_REVISION,
        rates_as_of: API_PRICING_RATES_AS_OF.to_string(),
        source_url: API_PRICING_SOURCE_URL.to_string(),
        basis: "current_api_rates_model_tokens_only".to_string(),
    }
}

pub fn format_api_cost_amount(cost: ApiCostAmount) -> String {
    if (cost.observed_samples > 0 || cost.observed_tokens > 0) && cost.priced_samples == 0 {
        return "-".to_string();
    }
    let minimum = format_pico_usd(cost.minimum_pico_usd);
    let mut formatted = if cost.range_is_exact() {
        minimum
    } else {
        format!("{minimum}–{}", format_pico_usd(cost.maximum_pico_usd))
    };
    if cost.priced_samples < cost.observed_samples || cost.priced_tokens < cost.observed_tokens {
        formatted.push('+');
    }
    formatted
}

pub fn format_api_equivalent_cost(cost: &ApiEquivalentCost) -> String {
    format_api_cost_amount(cost.amount)
}

pub fn format_pico_usd(value: PicoUsd) -> String {
    let value = value.value();
    if value > 0 && value < PICO_USD_PER_USD / 1_000_000 {
        return "<$0.000001".to_string();
    }
    let decimals = if value > 0 && value < PICO_USD_PER_USD / 10_000 {
        6
    } else if value < PICO_USD_PER_USD * 1_000 {
        4
    } else {
        2
    };
    let divisor = 10_u128.pow(12 - decimals);
    let rounded = value.saturating_add(divisor / 2) / divisor;
    let scale = 10_u128.pow(decimals);
    format!(
        "${}.{:0width$}",
        rounded / scale,
        rounded % scale,
        width = decimals as usize
    )
}

fn price_call(call: &UsageCall) -> CallCost {
    let observed_tokens = coverage_tokens(call.tokens);
    let Some(model_rates) = model_rates(call.model.as_deref()) else {
        return unpriced(observed_tokens, MODEL_UNKNOWN);
    };
    let Some(service_tier) = service_tier(call.service_tier.as_deref()) else {
        return unpriced(observed_tokens, SERVICE_TIER_UNKNOWN);
    };
    let tier_rates = match service_tier {
        ServiceTier::Standard => model_rates.standard,
        ServiceTier::Fast => {
            let Some(fast) = model_rates.fast else {
                return unpriced(observed_tokens, SERVICE_TIER_UNAVAILABLE);
            };
            fast
        }
    };
    let tokens = call.tokens;
    if tokens.total_tokens > 0 && tokens.input_tokens == 0 && tokens.output_tokens == 0 {
        return unpriced(observed_tokens, TOKEN_BREAKDOWN_MISSING);
    }
    if tokens.total_tokens > 0
        && tokens.total_tokens != tokens.input_tokens.saturating_add(tokens.output_tokens)
    {
        return unpriced(observed_tokens, TOKEN_BREAKDOWN_INCONSISTENT);
    }
    let Some(regular_input_tokens) = tokens
        .input_tokens
        .checked_sub(tokens.cached_input_tokens)
        .and_then(|value| value.checked_sub(tokens.cache_write_input_tokens))
    else {
        return unpriced(observed_tokens, TOKEN_BREAKDOWN_INCONSISTENT);
    };

    let Some(short_cost) = price_tokens(
        regular_input_tokens,
        tokens.cached_input_tokens,
        tokens.cache_write_input_tokens,
        tokens.output_tokens,
        tier_rates.short,
    ) else {
        return unpriced(observed_tokens, CACHE_WRITE_RATE_UNAVAILABLE);
    };
    if tokens.input_tokens <= LONG_CONTEXT_INPUT_THRESHOLD {
        return priced(observed_tokens, short_cost, short_cost, None);
    }

    let long_rates = match tier_rates.long {
        LongContextRates::Published(rates) => rates,
        LongContextRates::Flat => {
            return priced(observed_tokens, short_cost, short_cost, None);
        }
        LongContextRates::Unavailable => {
            return unpriced(observed_tokens, LONG_CONTEXT_UNAVAILABLE);
        }
    };
    let Some(long_cost) = price_tokens(
        regular_input_tokens,
        tokens.cached_input_tokens,
        tokens.cache_write_input_tokens,
        tokens.output_tokens,
        long_rates,
    ) else {
        return unpriced(observed_tokens, CACHE_WRITE_RATE_UNAVAILABLE);
    };
    if call.request_usage_exact {
        priced(observed_tokens, long_cost, long_cost, None)
    } else {
        priced(
            observed_tokens,
            short_cost.min(long_cost),
            short_cost.max(long_cost),
            Some(LONG_CONTEXT_AMBIGUOUS),
        )
    }
}

fn coverage_tokens(tokens: TokenUsage) -> u64 {
    tokens
        .input_tokens
        .saturating_add(tokens.output_tokens)
        .max(tokens.total_tokens)
}

fn price_tokens(
    input: u64,
    cached_input: u64,
    cache_write: u64,
    output: u64,
    rates: TokenRates,
) -> Option<u128> {
    let cache_write_cost = match (cache_write, rates.cache_write) {
        (0, _) => 0,
        (_, Some(rate)) => u128::from(cache_write).saturating_mul(rate),
        (_, None) => return None,
    };
    Some(
        u128::from(input)
            .saturating_mul(rates.input)
            .saturating_add(u128::from(cached_input).saturating_mul(rates.cached_input))
            .saturating_add(cache_write_cost)
            .saturating_add(u128::from(output).saturating_mul(rates.output)),
    )
}

fn priced(
    observed_tokens: u64,
    minimum_pico_usd: u128,
    maximum_pico_usd: u128,
    partial_reason: Option<&'static str>,
) -> CallCost {
    CallCost {
        minimum_pico_usd,
        maximum_pico_usd,
        observed_tokens,
        priced: true,
        partial_reason,
    }
}

fn unpriced(observed_tokens: u64, partial_reason: &'static str) -> CallCost {
    CallCost {
        minimum_pico_usd: 0,
        maximum_pico_usd: 0,
        observed_tokens,
        priced: false,
        partial_reason: Some(partial_reason),
    }
}

fn service_tier(value: Option<&str>) -> Option<ServiceTier> {
    let value = value.map(str::trim).filter(|value| !value.is_empty())?;
    if value.eq_ignore_ascii_case("default") || value.eq_ignore_ascii_case("standard") {
        Some(ServiceTier::Standard)
    } else if value.eq_ignore_ascii_case("fast") || value.eq_ignore_ascii_case("priority") {
        Some(ServiceTier::Fast)
    } else {
        None
    }
}

fn model_name(call: &UsageCall) -> &str {
    call.model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("unknown")
}

fn model_rates(model: Option<&str>) -> Option<ModelRates> {
    let model = model?.trim();
    if model.eq_ignore_ascii_case("gpt-5.6")
        || model.eq_ignore_ascii_case("gpt-5.6-sol")
        || model.eq_ignore_ascii_case("daybreak-blue-latest")
    {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(4_000_000, 400_000, Some(5_000_000), 20_000_000),
                long: LongContextRates::Published(TokenRates::new(
                    8_000_000,
                    800_000,
                    Some(10_000_000),
                    30_000_000,
                )),
            },
            fast: Some(TierRates {
                short: TokenRates::new(8_000_000, 800_000, Some(10_000_000), 40_000_000),
                long: LongContextRates::Published(TokenRates::new(
                    16_000_000,
                    1_600_000,
                    Some(20_000_000),
                    60_000_000,
                )),
            }),
        })
    } else if model.eq_ignore_ascii_case("gpt-5.6-terra") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(2_000_000, 200_000, Some(2_500_000), 12_000_000),
                long: LongContextRates::Published(TokenRates::new(
                    4_000_000,
                    400_000,
                    Some(5_000_000),
                    18_000_000,
                )),
            },
            fast: Some(TierRates {
                short: TokenRates::new(4_000_000, 400_000, Some(5_000_000), 24_000_000),
                long: LongContextRates::Published(TokenRates::new(
                    8_000_000,
                    800_000,
                    Some(10_000_000),
                    36_000_000,
                )),
            }),
        })
    } else if model.eq_ignore_ascii_case("gpt-5.6-luna") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(200_000, 20_000, Some(250_000), 1_200_000),
                long: LongContextRates::Published(TokenRates::new(
                    400_000,
                    40_000,
                    Some(500_000),
                    1_800_000,
                )),
            },
            fast: Some(TierRates {
                short: TokenRates::new(400_000, 40_000, Some(500_000), 2_400_000),
                long: LongContextRates::Published(TokenRates::new(
                    800_000,
                    80_000,
                    Some(1_000_000),
                    3_600_000,
                )),
            }),
        })
    } else if model.eq_ignore_ascii_case("gpt-5.5") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(5_000_000, 500_000, None, 30_000_000),
                long: LongContextRates::Published(TokenRates::new(
                    10_000_000, 1_000_000, None, 45_000_000,
                )),
            },
            fast: Some(TierRates {
                short: TokenRates::new(12_500_000, 1_250_000, None, 75_000_000),
                long: LongContextRates::Unavailable,
            }),
        })
    } else if model.eq_ignore_ascii_case("gpt-5.4") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(2_500_000, 250_000, None, 15_000_000),
                long: LongContextRates::Published(TokenRates::new(
                    5_000_000, 500_000, None, 22_500_000,
                )),
            },
            fast: Some(TierRates {
                short: TokenRates::new(5_000_000, 500_000, None, 30_000_000),
                long: LongContextRates::Unavailable,
            }),
        })
    } else if model.eq_ignore_ascii_case("gpt-5.4-mini") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(750_000, 75_000, None, 4_500_000),
                long: LongContextRates::Flat,
            },
            fast: Some(TierRates {
                short: TokenRates::new(1_500_000, 150_000, None, 9_000_000),
                long: LongContextRates::Flat,
            }),
        })
    } else if model.eq_ignore_ascii_case("gpt-5.3-codex") || model.eq_ignore_ascii_case("gpt-5.2") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(1_750_000, 175_000, None, 14_000_000),
                long: LongContextRates::Flat,
            },
            fast: Some(TierRates {
                short: TokenRates::new(3_500_000, 350_000, None, 28_000_000),
                long: LongContextRates::Flat,
            }),
        })
    } else if model.eq_ignore_ascii_case("gpt-5.2-codex") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(1_750_000, 175_000, None, 14_000_000),
                long: LongContextRates::Flat,
            },
            fast: None,
        })
    } else if model.eq_ignore_ascii_case("gpt-5.6-cyber")
        || model.eq_ignore_ascii_case("daybreak-red-latest")
    {
        // The catalog source's Cyber table currently publishes dashes for all
        // long-context cells. A model-page multiplier note conflicts with that
        // table, so long calls remain explicitly unpriced rather than guessed.
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(12_500_000, 1_250_000, Some(15_625_000), 75_000_000),
                long: LongContextRates::Unavailable,
            },
            fast: None,
        })
    } else if model.eq_ignore_ascii_case("gpt-5.5-cyber") {
        Some(ModelRates {
            standard: TierRates {
                short: TokenRates::new(12_500_000, 1_250_000, None, 75_000_000),
                long: LongContextRates::Unavailable,
            },
            fast: None,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn call(model: &str, service_tier: Option<&str>, tokens: TokenUsage) -> UsageCall {
        UsageCall {
            timestamp: Utc::now(),
            thread_id: "thread".to_string(),
            turn_id: Some("turn".to_string()),
            model: Some(model.to_string()),
            service_tier: Some(service_tier.unwrap_or("default").to_string()),
            tokens,
            request_usage_exact: true,
        }
    }

    fn assert_exact_price(
        model: &str,
        service_tier: Option<&str>,
        tokens: TokenUsage,
        expected_pico_usd: u128,
    ) {
        let cost = price_call(&call(model, service_tier, tokens));
        assert!(cost.priced, "{model} {service_tier:?} should be priced");
        assert_eq!(
            cost.minimum_pico_usd, expected_pico_usd,
            "{model} {service_tier:?} minimum"
        );
        assert_eq!(
            cost.maximum_pico_usd, expected_pico_usd,
            "{model} {service_tier:?} maximum"
        );
        assert_eq!(cost.partial_reason, None, "{model} {service_tier:?} reason");
    }

    fn assert_unpriced_price(
        model: &str,
        service_tier: Option<&str>,
        tokens: TokenUsage,
        expected_reason: &'static str,
    ) {
        let cost = price_call(&call(model, service_tier, tokens));
        assert!(!cost.priced, "{model} {service_tier:?} should be unpriced");
        assert_eq!(
            cost.partial_reason,
            Some(expected_reason),
            "{model} {service_tier:?} reason"
        );
    }

    #[test]
    fn standard_sol_prices_each_disjoint_token_component() {
        let priced = price_call(&call(
            "gpt-5.6-sol",
            None,
            TokenUsage {
                input_tokens: 100_000,
                cached_input_tokens: 20_000,
                cache_write_input_tokens: 10_000,
                output_tokens: 5_000,
                reasoning_output_tokens: 3_000,
                total_tokens: 105_000,
            },
        ));

        // 70k regular input + 20k cached + 10k cache write + 5k output.
        assert_eq!(priced.minimum_pico_usd, 438_000_000_000);
        assert_eq!(priced.maximum_pico_usd, priced.minimum_pico_usd);
    }

    #[test]
    fn gpt_5_6_family_short_and_long_prices_match_the_published_matrix() {
        let short = TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 20_000,
            cache_write_input_tokens: 10_000,
            output_tokens: 5_000,
            total_tokens: 105_000,
            ..TokenUsage::default()
        };
        let long = TokenUsage {
            input_tokens: 300_000,
            cached_input_tokens: 100_000,
            cache_write_input_tokens: 50_000,
            output_tokens: 20_000,
            total_tokens: 320_000,
            ..TokenUsage::default()
        };
        let cases = [
            (
                "gpt-5.6-sol",
                438_000_000_000,
                876_000_000_000,
                2_380_000_000_000,
                4_760_000_000_000,
            ),
            (
                "gpt-5.6-terra",
                229_000_000_000,
                458_000_000_000,
                1_250_000_000_000,
                2_500_000_000_000,
            ),
            (
                "gpt-5.6-luna",
                22_900_000_000,
                45_800_000_000,
                125_000_000_000,
                250_000_000_000,
            ),
        ];

        for (model, standard_short, fast_short, standard_long, fast_long) in cases {
            assert_exact_price(model, None, short, standard_short);
            assert_exact_price(model, Some("priority"), short, fast_short);
            assert_exact_price(model, None, long, standard_long);
            assert_exact_price(model, Some("fast"), long, fast_long);
        }
    }

    #[test]
    fn historical_models_use_their_published_standard_and_fast_short_prices() {
        let tokens = TokenUsage {
            input_tokens: 250_000,
            cached_input_tokens: 100_000,
            output_tokens: 20_000,
            total_tokens: 270_000,
            ..TokenUsage::default()
        };
        let cases = [
            ("gpt-5.5", 1_400_000_000_000, Some(3_500_000_000_000)),
            ("gpt-5.4", 700_000_000_000, Some(1_400_000_000_000)),
            ("gpt-5.4-mini", 210_000_000_000, Some(420_000_000_000)),
            ("gpt-5.3-codex", 560_000_000_000, Some(1_120_000_000_000)),
            ("gpt-5.2", 560_000_000_000, Some(1_120_000_000_000)),
            ("gpt-5.2-codex", 560_000_000_000, None),
        ];

        for (model, standard, fast) in cases {
            assert_exact_price(model, None, tokens, standard);
            if let Some(fast) = fast {
                assert_exact_price(model, Some("priority"), tokens, fast);
            } else {
                assert_unpriced_price(model, Some("priority"), tokens, SERVICE_TIER_UNAVAILABLE);
            }
        }
    }

    #[test]
    fn long_context_surcharges_and_flat_price_profiles_are_distinct() {
        let tokens = TokenUsage {
            input_tokens: 300_000,
            cached_input_tokens: 100_000,
            output_tokens: 20_000,
            total_tokens: 320_000,
            ..TokenUsage::default()
        };

        assert_exact_price("gpt-5.5", None, tokens, 3_000_000_000_000);
        assert_exact_price("gpt-5.4", None, tokens, 1_500_000_000_000);

        for (model, service_tier) in [("gpt-5.5", Some("priority")), ("gpt-5.4", Some("fast"))] {
            assert_unpriced_price(model, service_tier, tokens, LONG_CONTEXT_UNAVAILABLE);
        }

        for (model, service_tier, expected) in [
            ("gpt-5.4-mini", None, 247_500_000_000),
            ("gpt-5.4-mini", Some("fast"), 495_000_000_000),
            ("gpt-5.3-codex", None, 647_500_000_000),
            ("gpt-5.3-codex", Some("fast"), 1_295_000_000_000),
            ("gpt-5.2", None, 647_500_000_000),
            ("gpt-5.2", Some("fast"), 1_295_000_000_000),
            ("gpt-5.2-codex", None, 647_500_000_000),
        ] {
            assert_exact_price(model, service_tier, tokens, expected);
            let mut cumulative = call(model, service_tier, tokens);
            cumulative.request_usage_exact = false;
            let cost = price_call(&cumulative);
            assert_eq!(cost.minimum_pico_usd, expected);
            assert_eq!(cost.maximum_pico_usd, expected);
            assert_eq!(cost.partial_reason, None);
        }
    }

    #[test]
    fn cache_write_support_and_cyber_profiles_follow_the_public_rows() {
        let with_cache_write = TokenUsage {
            input_tokens: 250_000,
            cached_input_tokens: 100_000,
            cache_write_input_tokens: 50_000,
            output_tokens: 20_000,
            total_tokens: 270_000,
            ..TokenUsage::default()
        };
        assert_exact_price("gpt-5.6-cyber", None, with_cache_write, 3_656_250_000_000);
        assert_exact_price(
            "daybreak-red-latest",
            None,
            with_cache_write,
            3_656_250_000_000,
        );

        for model in ["gpt-5.5", "gpt-5.5-cyber"] {
            assert_unpriced_price(model, None, with_cache_write, CACHE_WRITE_RATE_UNAVAILABLE);
        }

        let without_cache_write = TokenUsage {
            cache_write_input_tokens: 0,
            ..with_cache_write
        };
        assert_exact_price(
            "gpt-5.5-cyber",
            None,
            without_cache_write,
            3_500_000_000_000,
        );
        assert_unpriced_price(
            "gpt-5.6-cyber",
            Some("priority"),
            without_cache_write,
            SERVICE_TIER_UNAVAILABLE,
        );

        let long = TokenUsage {
            input_tokens: 300_000,
            cached_input_tokens: 100_000,
            cache_write_input_tokens: 50_000,
            output_tokens: 20_000,
            total_tokens: 320_000,
            ..TokenUsage::default()
        };
        assert_unpriced_price("gpt-5.6-cyber", None, long, LONG_CONTEXT_UNAVAILABLE);
    }

    #[test]
    fn current_aliases_resolve_case_insensitively_without_reclassifying_legacy_cyber() {
        let sol_tokens = TokenUsage {
            input_tokens: 100_000,
            cached_input_tokens: 20_000,
            cache_write_input_tokens: 10_000,
            output_tokens: 5_000,
            total_tokens: 105_000,
            ..TokenUsage::default()
        };
        for alias in ["  GPT-5.6  ", "  DAYBREAK-BLUE-LATEST  "] {
            assert_exact_price(alias, None, sol_tokens, 438_000_000_000);
            assert_exact_price(alias, Some("FAST"), sol_tokens, 876_000_000_000);
        }

        let cyber_tokens = TokenUsage {
            input_tokens: 250_000,
            cached_input_tokens: 100_000,
            cache_write_input_tokens: 50_000,
            output_tokens: 20_000,
            total_tokens: 270_000,
            ..TokenUsage::default()
        };
        assert_exact_price(
            "  DAYBREAK-RED-LATEST  ",
            None,
            cyber_tokens,
            3_656_250_000_000,
        );
        assert_unpriced_price(
            "gpt-5.5-cyber",
            None,
            cyber_tokens,
            CACHE_WRITE_RATE_UNAVAILABLE,
        );
    }

    #[test]
    fn strict_long_context_boundary_and_ambiguous_range_are_preserved() {
        let tokens = |input_tokens| TokenUsage {
            input_tokens,
            total_tokens: input_tokens,
            ..TokenUsage::default()
        };
        let short = price_call(&call("gpt-5.6-sol", None, tokens(272_000)));
        let long = price_call(&call("gpt-5.6-sol", None, tokens(272_001)));
        let mut ambiguous_call = call("gpt-5.6-sol", None, tokens(272_001));
        ambiguous_call.request_usage_exact = false;
        let ambiguous = price_call(&ambiguous_call);

        assert_eq!(short.minimum_pico_usd, 1_088_000_000_000);
        assert_eq!(long.minimum_pico_usd, 2_176_008_000_000);
        assert_eq!(ambiguous.minimum_pico_usd, 1_088_004_000_000);
        assert_eq!(ambiguous.maximum_pico_usd, 2_176_008_000_000);
        assert_eq!(ambiguous.partial_reason, Some(LONG_CONTEXT_AMBIGUOUS));
    }

    #[test]
    fn fast_tier_uses_api_prices_and_unknown_inputs_remain_unpriced() {
        let tokens = TokenUsage {
            input_tokens: 1_000_000,
            total_tokens: 1_000_000,
            ..TokenUsage::default()
        };
        let fast = price_call(&call("gpt-5.6-terra", Some("priority"), tokens));
        assert_eq!(fast.minimum_pico_usd, 8_000_000_000_000);

        let unknown_model = price_call(&call("codex-auto-review", None, tokens));
        assert!(!unknown_model.priced);
        assert_eq!(unknown_model.partial_reason, Some(MODEL_UNKNOWN));

        let unknown_tier = price_call(&call("gpt-5.6-terra", Some("future"), tokens));
        assert!(!unknown_tier.priced);
        assert_eq!(unknown_tier.partial_reason, Some(SERVICE_TIER_UNKNOWN));

        let mut missing_tier = call("gpt-5.6-terra", None, tokens);
        missing_tier.service_tier = None;
        let missing_tier = price_call(&missing_tier);
        assert!(!missing_tier.priced);
        assert_eq!(missing_tier.partial_reason, Some(SERVICE_TIER_UNKNOWN));
    }

    #[test]
    fn missing_and_inconsistent_breakdowns_are_not_fabricated() {
        let missing = price_call(&call(
            "gpt-5.6-luna",
            None,
            TokenUsage {
                total_tokens: 1_000,
                ..TokenUsage::default()
            },
        ));
        assert!(!missing.priced);
        assert_eq!(missing.partial_reason, Some(TOKEN_BREAKDOWN_MISSING));

        let inconsistent = price_call(&call(
            "gpt-5.6-luna",
            None,
            TokenUsage {
                input_tokens: 100,
                cached_input_tokens: 80,
                cache_write_input_tokens: 30,
                total_tokens: 100,
                ..TokenUsage::default()
            },
        ));
        assert!(!inconsistent.priced);
        assert_eq!(
            inconsistent.partial_reason,
            Some(TOKEN_BREAKDOWN_INCONSISTENT)
        );

        let mismatched_total = price_call(&call(
            "gpt-5.6-luna",
            None,
            TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                total_tokens: 100,
                ..TokenUsage::default()
            },
        ));
        assert!(!mismatched_total.priced);
        assert_eq!(
            mismatched_total.partial_reason,
            Some(TOKEN_BREAKDOWN_INCONSISTENT)
        );
    }

    #[test]
    fn zero_token_coverage_falls_back_to_sample_coverage() {
        let empty = ApiCostAmount::default();
        assert_eq!(empty.priced_token_percent(), 100.0);

        let unpriced_zero_token_call = ApiCostAmount {
            observed_samples: 1,
            ..ApiCostAmount::default()
        };
        assert_eq!(unpriced_zero_token_call.priced_token_percent(), 0.0);
    }

    #[test]
    fn aggregation_reports_coverage_without_turning_unknown_cost_into_zero() {
        let tokens = TokenUsage {
            input_tokens: 1_000,
            total_tokens: 1_000,
            ..TokenUsage::default()
        };
        let mut aggregation = ApiCostAggregation::default();
        aggregation.add_call(&call("gpt-5.6-luna", None, tokens));
        aggregation.add_call(&call("unknown", None, tokens));
        let summary = aggregation.total();

        assert_eq!(summary.amount.observed_samples, 2);
        assert_eq!(summary.amount.priced_samples, 1);
        assert_eq!(summary.amount.priced_token_percent(), 50.0);
        assert!(summary.is_partial());
        assert_eq!(summary.partial_reasons, vec![MODEL_UNKNOWN.to_string()]);
        assert_eq!(summary.model_breakdown.len(), 2);
        assert_eq!(summary.model_breakdown[0].model, "gpt-5.6-luna");
        assert_eq!(summary.model_breakdown[0].amount.priced_samples, 1);
        assert_eq!(summary.model_breakdown[1].model, "unknown");
        assert_eq!(summary.model_breakdown[1].amount.priced_samples, 0);
        let mut reconciled = ApiCostAmount::default();
        for model in &summary.model_breakdown {
            reconciled.add_assign(model.amount);
        }
        assert_eq!(reconciled, summary.amount);
        assert_eq!(format_api_equivalent_cost(&summary), "$0.0002+");
    }

    #[test]
    fn money_format_distinguishes_tiny_zero_normal_and_large_values() {
        assert_eq!(format_pico_usd(PicoUsd::new(0)), "$0.0000");
        assert_eq!(format_pico_usd(PicoUsd::new(1)), "<$0.000001");
        assert_eq!(format_pico_usd(PicoUsd::new(1_234_567)), "$0.000001");
        assert_eq!(
            format_pico_usd(PicoUsd::new(12_345_600_000_000)),
            "$12.3456"
        );
        assert_eq!(
            format_pico_usd(PicoUsd::new(12_345_678_000_000_000)),
            "$12345.68"
        );
    }

    #[test]
    fn pico_usd_serializes_as_an_exact_decimal_string() {
        let value = PicoUsd::new(u64::MAX as u128 + 1);
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, format!("\"{}\"", value.value()));
        assert_eq!(serde_json::from_str::<PicoUsd>(&json).unwrap(), value);
    }
}
