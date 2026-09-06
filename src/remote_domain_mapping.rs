//! Lossless conversion from validated remote protocol values into local
//! accounting domain values.
//!
//! Protocol validation remains at the transport boundary. These helpers only
//! centralize the mechanical, field-for-field mapping used by the aggregate
//! and per-event ingest paths so the two cannot silently drift apart.

use crate::domain::{ApiCostAmount, PicoUsd, TokenUsage};
use crate::remote_protocol::{RemoteApiCostAmount, RemoteSessionUsageMetrics, RemoteTokenUsage};
use crate::source_history::SessionUsageMetrics;

pub(crate) fn local_token_usage(usage: RemoteTokenUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        total_tokens: usage.total_tokens,
    }
}

pub(crate) fn local_api_cost(cost: RemoteApiCostAmount) -> ApiCostAmount {
    ApiCostAmount {
        minimum_pico_usd: PicoUsd::new(cost.minimum_pico_usd.value()),
        maximum_pico_usd: PicoUsd::new(cost.maximum_pico_usd.value()),
        observed_samples: cost.observed_samples,
        priced_samples: cost.priced_samples,
        observed_tokens: cost.observed_tokens,
        priced_tokens: cost.priced_tokens,
    }
}

pub(crate) fn local_session_usage_metrics(
    metrics: RemoteSessionUsageMetrics,
) -> SessionUsageMetrics {
    SessionUsageMetrics {
        token_usage: local_token_usage(metrics.token_usage),
        estimated_cost_units: metrics.estimated_cost_units.value(),
        api_long_context_extra_cost_units: metrics
            .api_long_context_extra_cost_units
            .map(|value| value.value()),
        api_equivalent_cost: local_api_cost(metrics.api_equivalent_cost),
        call_count: metrics.call_count,
        metric_revision: metrics.metric_revision.get(),
        estimator_revision: metrics.estimator_revision.get(),
        project_breakdown_revision: metrics.project_breakdown_revision.get(),
        api_pricing_catalog_revision: metrics.api_pricing_catalog_revision.get(),
        partial_reasons: metrics.partial_reasons,
    }
}

pub(crate) fn local_session_usage_metrics_ref(
    metrics: &RemoteSessionUsageMetrics,
) -> SessionUsageMetrics {
    local_session_usage_metrics(metrics.clone())
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::*;
    use crate::remote_protocol::RemoteU128;

    #[test]
    fn session_metrics_mapping_preserves_every_accounting_field() {
        let remote = RemoteSessionUsageMetrics {
            token_usage: RemoteTokenUsage {
                input_tokens: 11,
                cached_input_tokens: 7,
                cache_write_input_tokens: 3,
                output_tokens: 5,
                reasoning_output_tokens: 2,
                total_tokens: 16,
            },
            estimated_cost_units: RemoteU128::new(17),
            api_long_context_extra_cost_units: Some(RemoteU128::new(19)),
            api_equivalent_cost: RemoteApiCostAmount {
                minimum_pico_usd: RemoteU128::new(23),
                maximum_pico_usd: RemoteU128::new(29),
                observed_samples: 37,
                priced_samples: 31,
                observed_tokens: 43,
                priced_tokens: 41,
            },
            call_count: 47,
            metric_revision: NonZeroU32::new(2).unwrap(),
            estimator_revision: NonZeroU32::new(3).unwrap(),
            project_breakdown_revision: NonZeroU32::new(5).unwrap(),
            api_pricing_catalog_revision: NonZeroU32::new(7).unwrap(),
            partial_reasons: vec!["sample-partial".to_owned()],
        };

        let local = local_session_usage_metrics(remote.clone());

        assert_eq!(local.token_usage, local_token_usage(remote.token_usage));
        assert_eq!(local.estimated_cost_units, 17);
        assert_eq!(local.api_long_context_extra_cost_units, Some(19));
        assert_eq!(
            local.api_equivalent_cost,
            local_api_cost(remote.api_equivalent_cost)
        );
        assert_eq!(local.call_count, 47);
        assert_eq!(local.metric_revision, 2);
        assert_eq!(local.estimator_revision, 3);
        assert_eq!(local.project_breakdown_revision, 5);
        assert_eq!(local.api_pricing_catalog_revision, 7);
        assert_eq!(local.partial_reasons, ["sample-partial"]);
    }
}
