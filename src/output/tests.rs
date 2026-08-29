use super::*;
use crate::domain::{
    ApiCostAmount, ApiEquivalentCost, ApiModelCost, ApiPricingMetadata, AttributionSummary,
    CollectionStats, LimitBucket, ModelUsage, PicoUsd, Provenance, RateLimitResetCredit,
    RateLimitResetCreditsSnapshot, SourceStatus, TaskRecord, TaskStatus, ThreadWindowUsage,
    TurnRecord, TurnStatus, TurnWindowUsage, WindowAnalysis, WindowDescriptor, WindowUsage,
};

#[test]
fn compact_token_units_are_stable() {
    assert_eq!(
        compact_tokens(TokenUsage {
            total_tokens: 12_345,
            ..TokenUsage::default()
        }),
        "12.3K"
    );
}

#[test]
fn every_available_estimate_is_marked_as_approximate() {
    assert_eq!(estimated_percent(2.26, Confidence::Unknown), "-");
    assert_eq!(estimated_percent(99.0, Confidence::Unknown), "-");
    assert_eq!(estimated_percent(0.0, Confidence::Low), "~0.00%");
    assert_eq!(estimated_percent(2.26, Confidence::Low), "~2.26%");
    assert_eq!(estimated_percent(2.26, Confidence::Medium), "~2.26%");
    assert_eq!(estimated_percent(2.26, Confidence::High), "~2.26%");
}

#[test]
fn attribution_text_describes_credit_rate_weighted_codex_gauge_estimate() {
    let now = Utc::now();
    let attribution = AttributionSummary {
        window: Some(WindowDescriptor {
            limit_id: "codex".to_string(),
            label: "week".to_string(),
            starts_at: now - chrono::Duration::days(7),
            ends_at: now,
            used_percent: 34.0,
        }),
        local_token_usage: TokenUsage {
            total_tokens: 760_000_000,
            ..TokenUsage::default()
        },
        observed_delta_percent: 4.0,
        estimated_assigned_percent: 4.0,
        proxy_projected_percent: 34.0,
        unattributed_percent: 30.0,
        attribution_coverage_percent: 11.8,
        confidence: Confidence::Low,
        method: "current_codex_gauge_credit_rate_weighted_proxy".to_string(),
        ..AttributionSummary::default()
    };

    let allocation = attribution_allocation_line(&attribution);
    assert!(allocation.contains("token total 760.00M"));
    assert!(allocation.contains("estimated ~34.00pp"));
    assert!(allocation.contains("codex gauge 34.00% used"));
    assert!(allocation.contains("gauge x credit-rate share"));
    assert!(!allocation.contains("observed"));
    assert!(!allocation.contains("evidence"));
    assert!(!allocation.contains("gap"));
    assert!(!allocation.contains("unattributed"));

    let quality = attribution_quality_line(&attribution);
    assert!(quality.contains("credit-rate-weighted quota proxy"));
    assert!(quality.contains("not server per-task accounting"));
    assert!(!quality.contains("confidence"));
    assert!(quality.contains("normal Codex bucket only (Spark excluded)"));
    assert!(!quality.contains("coverage"));
    assert!(!quality.contains("observed"));
}

#[test]
fn attribution_text_explains_unavailable_inputs() {
    let unavailable = AttributionSummary {
        local_token_usage: TokenUsage {
            total_tokens: 42,
            ..TokenUsage::default()
        },
        ..AttributionSummary::default()
    };
    assert!(attribution_allocation_line(&unavailable).contains("codex quota window unavailable"));

    let no_denominator = AttributionSummary {
        window: Some(WindowDescriptor {
            limit_id: "codex".to_string(),
            label: "5h".to_string(),
            starts_at: Utc::now() - chrono::Duration::hours(5),
            ends_at: Utc::now(),
            used_percent: 12.0,
        }),
        method: "codex_gauge_without_local_tokens".to_string(),
        ..AttributionSummary::default()
    };
    let allocation = attribution_allocation_line(&no_denominator);
    assert!(allocation.contains("codex gauge 12.00% used"));
    assert!(allocation.contains("estimated - (no token denominator)"));
}

#[test]
fn zero_percent_codex_gauge_is_still_a_known_estimate() {
    let now = Utc::now();
    let attribution = AttributionSummary {
        window: Some(WindowDescriptor {
            limit_id: "codex".to_string(),
            label: "week".to_string(),
            starts_at: now - chrono::Duration::days(7),
            ends_at: now,
            used_percent: 0.0,
        }),
        local_token_usage: TokenUsage {
            total_tokens: 1_000,
            ..TokenUsage::default()
        },
        proxy_projected_percent: 0.0,
        confidence: Confidence::Low,
        method: "current_codex_gauge_credit_rate_weighted_proxy".to_string(),
        ..AttributionSummary::default()
    };

    let allocation = attribution_allocation_line(&attribution);
    assert!(allocation.contains("estimated ~0.00pp"));
    assert!(!allocation.contains("unavailable"));
}

#[test]
fn proxy_projection_json_is_camel_case_and_backward_compatible() {
    let attribution = AttributionSummary {
        proxy_projected_percent: 30.0,
        ..AttributionSummary::default()
    };
    let mut value = serde_json::to_value(&attribution).unwrap();
    assert_eq!(value["proxyProjectedPercent"], 30.0);
    assert!(value.get("proxy_projected_percent").is_none());

    value
        .as_object_mut()
        .unwrap()
        .remove("proxyProjectedPercent");
    let legacy: AttributionSummary = serde_json::from_value(value).unwrap();
    assert_eq!(legacy.proxy_projected_percent, 0.0);
}

#[test]
fn token_usage_json_uses_camel_case() {
    let value = serde_json::to_value(TokenUsage {
        cache_write_input_tokens: 7,
        total_tokens: 42,
        ..TokenUsage::default()
    })
    .unwrap();
    assert_eq!(value["totalTokens"], 42);
    assert_eq!(value["cacheWriteInputTokens"], 7);
    assert!(value.get("total_tokens").is_none());
    assert!(value.get("cache_write_input_tokens").is_none());
}

#[test]
fn legacy_token_usage_json_defaults_missing_cache_write_to_zero() {
    let usage: TokenUsage = serde_json::from_value(serde_json::json!({
        "inputTokens": 40,
        "cachedInputTokens": 10,
        "outputTokens": 2,
        "reasoningOutputTokens": 1,
        "totalTokens": 42
    }))
    .unwrap();

    assert_eq!(usage.cache_write_input_tokens, 0);
}

#[test]
fn partial_status_is_scoped_to_requested_sections() {
    let now = Utc::now();
    let window_usage = WindowUsage {
        token_usage: TokenUsage {
            total_tokens: 42,
            ..TokenUsage::default()
        },
        local_token_share_percent: 100.0,
        estimated_quota_percent: 1.25,
        quota_confidence: Confidence::Medium,
        api_equivalent_cost: Default::default(),
    };
    let snapshot = Snapshot {
        schema_version: 1,
        api_pricing: Default::default(),
        api_equivalent_cost: Default::default(),
        as_of: now,
        partial: true,
        codex_home: "/tmp/.codex".into(),
        sources: vec![
            SourceStatus {
                source: "rollout_jsonl".to_string(),
                status: "ok".to_string(),
                as_of: now,
                message: None,
            },
            SourceStatus {
                source: "app_server".to_string(),
                status: "error".to_string(),
                as_of: now,
                message: Some("unavailable".to_string()),
            },
        ],
        limits: vec![LimitBucket {
            limit_id: "codex".to_string(),
            limit_name: None,
            plan_type: None,
            primary: Some(LimitWindow::new(
                10.0,
                Some(300),
                Some(now + chrono::Duration::hours(1)),
            )),
            secondary: None,
            credits: None,
            rate_limit_reached_type: None,
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        }],
        rate_limit_reset_credits: Some(RateLimitResetCreditsSnapshot {
            available_count: 3,
            credits: Some(vec![RateLimitResetCredit {
                granted_at: now - chrono::Duration::hours(2),
                expires_at: None,
                status: "available".to_string(),
                reset_type: "codexRateLimits".to_string(),
                title: Some("Reset\u{1b}[2J Codex limits".to_string()),
                description: Some("One reset opportunity".to_string()),
            }]),
            provenance: Provenance::ServerSnapshot,
            as_of: now,
        }),
        rate_limit_reset_credits_partial: false,
        account_usage: None,
        tasks: vec![TaskRecord {
            thread_id: "task-thread".to_string(),
            parent_thread_id: None,
            archived: false,
            title: "task".to_string(),
            cwd: None,
            source: Some("desktop".to_string()),
            created_at: None,
            updated_at: None,
            status: TaskStatus::Completed,
            status_provenance: Provenance::LocalExact,
            status_confidence: Confidence::High,
            token_usage: TokenUsage::default(),
            turn_count: 1,
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: Default::default(),
        }],
        turns: vec![TurnRecord {
            thread_id: "task-thread".to_string(),
            turn_id: "task-turn".to_string(),
            model: Some("gpt-test".to_string()),
            reasoning_effort: Some("xhigh".to_string()),
            service_tier: Some("priority".to_string()),
            message_preview: Some("message preview".to_string()),
            started_at: None,
            completed_at: None,
            duration_ms: None,
            status: TurnStatus::InProgress,
            token_usage: TokenUsage::default(),
            window_token_usage: TokenUsage::default(),
            local_token_share_percent: 0.0,
            estimated_quota_percent: 0.0,
            quota_confidence: Confidence::Unknown,
            api_equivalent_cost: Default::default(),
        }],
        models: vec![ModelUsage {
            model: "gpt-test".to_string(),
            token_usage: window_usage.token_usage,
            local_token_share_percent: 100.0,
            estimated_quota_percent: 1.25,
            quota_confidence: Confidence::Medium,
            api_equivalent_cost: Default::default(),
        }],
        attribution: AttributionSummary {
            window: Some(WindowDescriptor {
                limit_id: "codex".to_string(),
                label: "5h".to_string(),
                starts_at: now - chrono::Duration::hours(4),
                ends_at: now + chrono::Duration::hours(1),
                used_percent: 10.0,
            }),
            local_token_usage: window_usage.token_usage,
            proxy_projected_percent: 10.0,
            external_activity_possible: true,
            confidence: Confidence::Medium,
            method: "current_codex_gauge_credit_rate_weighted_proxy".to_string(),
            ..AttributionSummary::default()
        },
        window_analyses: vec![WindowAnalysis {
            duration_mins: 10_080,
            attribution: AttributionSummary {
                window: Some(WindowDescriptor {
                    limit_id: "codex".to_string(),
                    label: "week".to_string(),
                    starts_at: now - chrono::Duration::days(5),
                    ends_at: now + chrono::Duration::days(2),
                    used_percent: 23.0,
                }),
                local_token_usage: window_usage.token_usage,
                method: "local_tokens_only".to_string(),
                ..AttributionSummary::default()
            },
            partial: false,
            partial_reasons: Vec::new(),
            threads: vec![ThreadWindowUsage {
                thread_id: "task-thread".to_string(),
                usage: window_usage,
            }],
            turns: vec![TurnWindowUsage {
                thread_id: "task-thread".to_string(),
                turn_id: "task-turn".to_string(),
                usage: window_usage,
            }],
            models: vec![ModelUsage {
                model: "gpt-test".to_string(),
                token_usage: window_usage.token_usage,
                local_token_share_percent: 100.0,
                estimated_quota_percent: 1.25,
                quota_confidence: Confidence::Medium,
                api_equivalent_cost: Default::default(),
            }],
            api_equivalent_cost: Default::default(),
            api_pricing: Default::default(),
            api_long_context: None,
        }],
        stats: CollectionStats::default(),
        warnings: Vec::new(),
        errors: vec!["app-server unavailable".to_string()],
    };
    let tasks = OutputRequest {
        format: OutputFormat::Json,
        compact: true,
        api_long_context: false,
        sections: BTreeSet::from([Section::Tasks]),
        thread_filter: None,
    };
    let limits = OutputRequest {
        sections: BTreeSet::from([Section::Limits]),
        ..tasks.clone()
    };

    assert!(request_is_partial(&snapshot, &tasks));
    assert!(!request_is_failure(&snapshot, &tasks));
    assert!(request_is_partial(&snapshot, &limits));
    assert!(!request_is_failure(&snapshot, &limits));
    let tasks_json: Value =
        serde_json::from_str(&render_output(&snapshot, &tasks).unwrap()).unwrap();
    assert_eq!(tasks_json["partial"], true);
    assert_eq!(tasks_json["tasks"][0]["source"], "desktop");
    assert!(tasks_json.get("windowAnalyses").is_none());
    assert!(tasks_json.get("accountUsage").is_none());
    assert!(tasks_json.get("rateLimitResetCredits").is_none());
    assert!(tasks_json.get("errors").is_some());
    assert!(tasks_json.get("estimateProjection").is_none());
    let long_context_request = OutputRequest {
        api_long_context: true,
        ..tasks.clone()
    };
    let long_context_json: Value =
        serde_json::from_str(&render_output(&snapshot, &long_context_request).unwrap()).unwrap();
    assert_eq!(long_context_json["estimateProjection"], "apiLongContext");
    let long_context_text = render_output(
        &snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            ..long_context_request
        },
    )
    .unwrap();
    assert!(long_context_text.contains("[EST LONGX]"));
    let turns_json: Value = serde_json::from_str(
        &render_output(
            &snapshot,
            &OutputRequest {
                sections: BTreeSet::from([Section::Turns]),
                ..tasks.clone()
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(turns_json["turns"][0]["messagePreview"], "message preview");
    assert_eq!(turns_json["turns"][0]["status"], "in_progress");
    assert_eq!(turns_json["turns"][0]["serviceTier"], "priority");
    let limits_json: Value =
        serde_json::from_str(&render_output(&snapshot, &limits).unwrap()).unwrap();
    assert_eq!(limits_json["partial"], true);
    assert_eq!(limits_json["rateLimitResetCredits"]["availableCount"], 3);
    assert_eq!(
        limits_json["rateLimitResetCredits"]["provenance"],
        "server_snapshot"
    );
    let reset_credit = &limits_json["rateLimitResetCredits"]["credits"][0];
    assert!(reset_credit["grantedAt"].is_string());
    assert!(reset_credit["expiresAt"].is_null());
    assert_eq!(reset_credit["status"], "available");
    assert_eq!(reset_credit["resetType"], "codexRateLimits");
    assert!(reset_credit.get("id").is_none());
    assert!(limits_json.get("errors").is_some());
    let limits_text = render_output(
        &snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            api_long_context: false,
            sections: BTreeSet::from([Section::Limits]),
            thread_filter: None,
        },
    )
    .unwrap();
    assert!(limits_text.contains("reset credits  3 available"));
    assert!(limits_text.contains("reset time never"));
    assert!(limits_text.contains("reset credit details  showing 1/3"));
    assert!(limits_text.contains("Reset [2J Codex limits"));
    assert!(!limits_text.contains('\u{1b}'));

    let mut unknown_reset_credit_details = snapshot.clone();
    unknown_reset_credit_details
        .rate_limit_reset_credits
        .as_mut()
        .unwrap()
        .credits = None;
    let unknown_details_json: Value =
        serde_json::from_str(&render_output(&unknown_reset_credit_details, &limits).unwrap())
            .unwrap();
    assert!(
        unknown_details_json["rateLimitResetCredits"]["credits"].is_null(),
        "None must remain JSON null so it is distinguishable from a fetched empty list"
    );

    let mut empty_reset_credit_details = snapshot.clone();
    let empty_summary = empty_reset_credit_details
        .rate_limit_reset_credits
        .as_mut()
        .unwrap();
    empty_summary.available_count = 0;
    empty_summary.credits = Some(Vec::new());
    let empty_details_json: Value =
        serde_json::from_str(&render_output(&empty_reset_credit_details, &limits).unwrap())
            .unwrap();
    assert_eq!(
        empty_details_json["rateLimitResetCredits"]["credits"],
        serde_json::json!([])
    );
    let empty_details_text = render_output(
        &empty_reset_credit_details,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            api_long_context: false,
            sections: BTreeSet::from([Section::Limits]),
            thread_filter: None,
        },
    )
    .unwrap();
    assert!(empty_details_text.contains("reset credit details  fetched, none returned"));

    let mut legacy_value = serde_json::to_value(&snapshot).unwrap();
    legacy_value["rateLimitResetCredits"]
        .as_object_mut()
        .unwrap()
        .remove("credits");
    let legacy_snapshot: Snapshot = serde_json::from_value(legacy_value).unwrap();
    assert!(
        legacy_snapshot
            .rate_limit_reset_credits
            .unwrap()
            .credits
            .is_none(),
        "old cached summaries without credits must deserialize as detail-unavailable"
    );

    let mut stale_reset_credits_only = snapshot.clone();
    stale_reset_credits_only.partial = false;
    stale_reset_credits_only.errors.clear();
    for source in &mut stale_reset_credits_only.sources {
        source.status = "ok".to_string();
        source.message = None;
    }
    stale_reset_credits_only
        .rate_limit_reset_credits
        .as_mut()
        .unwrap()
        .provenance = Provenance::Stale;
    assert!(request_is_partial(&stale_reset_credits_only, &limits));
    assert!(!request_is_partial(&stale_reset_credits_only, &tasks));

    let mut invalid_reset_credits_only = stale_reset_credits_only.clone();
    invalid_reset_credits_only.rate_limit_reset_credits = None;
    invalid_reset_credits_only.rate_limit_reset_credits_partial = true;
    invalid_reset_credits_only
        .warnings
        .push("invalid reset credits".to_string());
    assert!(request_is_partial(&invalid_reset_credits_only, &limits));
    assert!(!request_is_partial(&invalid_reset_credits_only, &tasks));
    let invalid_limits_json: Value =
        serde_json::from_str(&render_output(&invalid_reset_credits_only, &limits).unwrap())
            .unwrap();
    assert_eq!(invalid_limits_json["partial"], true);
    assert!(
        invalid_limits_json["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str() == Some("invalid reset credits"))
    );
    assert!(
        invalid_limits_json
            .get("rateLimitResetCreditsPartial")
            .is_none()
    );

    let text = render_output(
        &snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            api_long_context: false,
            sections: BTreeSet::from([Section::Tasks, Section::Turns]),
            thread_filter: None,
        },
    )
    .unwrap();
    assert!(text.contains("desktop"));
    assert!(text.contains("xhigh"));
    assert!(text.contains("in_progress"));
    assert!(text.contains("message preview"));
    assert!(text.contains("TOKEN5H%"));
    assert!(text.contains("TOKEN%"));
    assert!(!text.contains("LOCAL5H"));
    assert_eq!(text.matches("credit-rate-weighted quota proxy").count(), 1);
    assert!(text.contains("external activity possible true"));
    assert!(!text.contains("confidence Medium"));

    let turns_text = render_output(
        &snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            api_long_context: false,
            sections: BTreeSet::from([Section::Turns]),
            thread_filter: None,
        },
    )
    .unwrap();
    assert!(turns_text.contains("credit-rate-weighted quota proxy"));
    assert!(turns_text.contains("external activity possible true"));

    let models_text = render_output(
        &snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            api_long_context: false,
            sections: BTreeSet::from([Section::Models]),
            thread_filter: None,
        },
    )
    .unwrap();
    assert!(models_text.contains("~1.25% estimated quota"));
    assert!(models_text.contains("credit-rate-weighted quota proxy"));
    assert!(models_text.contains("not server per-task accounting"));
    assert!(models_text.contains("external activity possible true"));
    assert!(!models_text.contains("Medium"));
    assert!(!models_text.contains("confidence"));

    let mut cost_snapshot = snapshot.clone();
    cost_snapshot.partial = false;
    cost_snapshot.errors.clear();
    for source in &mut cost_snapshot.sources {
        source.status = "ok".to_string();
        source.message = None;
    }
    cost_snapshot.api_pricing = ApiPricingMetadata {
        catalog_revision: 1,
        rates_as_of: "2026-08-27".to_string(),
        source_url: "https://developers.openai.com/api/docs/pricing".to_string(),
        basis: "current_api_rates_model_tokens_only".to_string(),
    };
    let priced_amount = ApiCostAmount {
        minimum_pico_usd: PicoUsd::new(438_000_000_000),
        maximum_pico_usd: PicoUsd::new(438_000_000_000),
        observed_samples: 1,
        priced_samples: 1,
        observed_tokens: 135_000,
        priced_tokens: 135_000,
    };
    cost_snapshot.api_equivalent_cost = Some(ApiEquivalentCost {
        amount: ApiCostAmount {
            observed_samples: 2,
            priced_samples: 1,
            observed_tokens: 270_000,
            priced_tokens: 135_000,
            ..priced_amount
        },
        partial_reasons: vec!["api_price_model_unknown".to_string()],
        model_breakdown: vec![ApiModelCost {
            model: "gpt-test".to_string(),
            amount: priced_amount,
        }],
    });
    cost_snapshot.tasks[0].api_equivalent_cost = Some(priced_amount);
    cost_snapshot.turns[0].api_equivalent_cost = Some(priced_amount);
    cost_snapshot.models[0].api_equivalent_cost = priced_amount;
    let cost_analysis = &mut cost_snapshot.window_analyses[0];
    cost_analysis.duration_mins = 300;
    cost_analysis.api_pricing = cost_snapshot.api_pricing.clone();
    cost_analysis.api_equivalent_cost = ApiEquivalentCost {
        amount: ApiCostAmount {
            observed_samples: 2,
            priced_samples: 1,
            observed_tokens: 270_000,
            priced_tokens: 135_000,
            ..priced_amount
        },
        partial_reasons: vec!["api_price_model_unknown".to_string()],
        model_breakdown: vec![ApiModelCost {
            model: "gpt-test".to_string(),
            amount: priced_amount,
        }],
    };
    cost_analysis.models[0].api_equivalent_cost = priced_amount;
    cost_analysis.threads[0].usage.api_equivalent_cost = priced_amount;
    cost_analysis.turns[0].usage.api_equivalent_cost = priced_amount;
    assert!(!request_is_partial(
        &cost_snapshot,
        &OutputRequest {
            sections: BTreeSet::from([Section::Models]),
            ..tasks.clone()
        }
    ));
    let cost_text = render_output(
        &cost_snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            api_long_context: false,
            sections: BTreeSet::from([Section::Models]),
            thread_filter: None,
        },
    )
    .unwrap();
    assert!(cost_text.contains("API equivalent $0.4380"));
    assert!(cost_text.contains("model calls only"));
    assert!(cost_text.contains("50.0% priced"));
    assert!(cost_text.contains("api_price_model_unknown"));

    let cost_json: Value = serde_json::from_str(
        &render_output(
            &cost_snapshot,
            &OutputRequest {
                format: OutputFormat::Json,
                compact: true,
                api_long_context: false,
                sections: BTreeSet::from([Section::Models]),
                thread_filter: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(cost_json["apiPricing"]["catalogRevision"], 1);
    assert_eq!(cost_json["apiEquivalentCost"]["observedSamples"], 2);
    assert!(
        cost_json["apiEquivalentCost"]
            .get("observedCalls")
            .is_none()
    );
    assert_eq!(
        cost_json["apiEquivalentCost"]["minimumPicoUsd"],
        "438000000000"
    );
    assert_eq!(
        cost_json["models"][0]["apiEquivalentCost"]["minimumPicoUsd"],
        "438000000000"
    );
    assert_eq!(
        cost_json["apiEquivalentCost"]["modelBreakdown"][0]["model"],
        "gpt-test"
    );
    for section in [Section::Tasks, Section::Turns, Section::Attribution] {
        let section_json: Value = serde_json::from_str(
            &render_output(
                &cost_snapshot,
                &OutputRequest {
                    format: OutputFormat::Json,
                    compact: true,
                    api_long_context: false,
                    sections: BTreeSet::from([section]),
                    thread_filter: None,
                },
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(section_json["apiEquivalentCost"]["pricedTokens"], 135_000);
        assert!(section_json.get("apiPricing").is_some());
    }
    let tasks_json: Value = serde_json::from_str(
        &render_output(
            &cost_snapshot,
            &OutputRequest {
                format: OutputFormat::Json,
                compact: true,
                api_long_context: false,
                sections: BTreeSet::from([Section::Tasks]),
                thread_filter: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        tasks_json["tasks"][0]["apiEquivalentCost"]["minimumPicoUsd"],
        "438000000000"
    );
    let filtered_cost_request = OutputRequest {
        format: OutputFormat::Json,
        compact: true,
        api_long_context: false,
        sections: BTreeSet::from([Section::Turns]),
        thread_filter: Some("task-thread".to_string()),
    };
    let filtered_cost_json: Value =
        serde_json::from_str(&render_output(&cost_snapshot, &filtered_cost_request).unwrap())
            .unwrap();
    assert!(filtered_cost_json.get("apiEquivalentCost").is_none());
    assert_eq!(
        filtered_cost_json["turns"][0]["apiEquivalentCost"]["minimumPicoUsd"],
        "438000000000"
    );
    let filtered_cost_text = render_output(
        &cost_snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            ..filtered_cost_request
        },
    )
    .unwrap();
    assert!(!filtered_cost_text.contains("API equivalent $0.4380"));
    assert!(filtered_cost_text.contains("API.EQ5H"));
    assert!(filtered_cost_text.contains("$0.4380"));
    let limits_only_json: Value = serde_json::from_str(
        &render_output(
            &cost_snapshot,
            &OutputRequest {
                format: OutputFormat::Json,
                compact: true,
                api_long_context: false,
                sections: BTreeSet::from([Section::Limits]),
                thread_filter: None,
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert!(limits_only_json.get("apiPricing").is_none());
    assert!(limits_only_json.get("apiEquivalentCost").is_none());

    let mut five_hour_partial = snapshot.clone();
    five_hour_partial.partial = false;
    five_hour_partial.errors.clear();
    for source in &mut five_hour_partial.sources {
        source.status = "ok".to_string();
        source.message = None;
    }
    five_hour_partial.window_analyses[0].duration_mins = 300;
    five_hour_partial.window_analyses[0].partial = true;
    five_hour_partial.window_analyses[0]
        .partial_reasons
        .push("multiple_active_limit_buckets".to_string());
    for section in [
        Section::Tasks,
        Section::Turns,
        Section::Models,
        Section::Attribution,
    ] {
        assert!(request_is_partial(
            &five_hour_partial,
            &OutputRequest {
                sections: BTreeSet::from([section]),
                ..tasks.clone()
            }
        ));
    }

    let windows = OutputRequest {
        sections: BTreeSet::from([Section::Windows]),
        ..tasks.clone()
    };
    assert!(request_is_partial(&snapshot, &windows));
    assert!(!request_is_failure(&snapshot, &windows));
    let windows_json: Value =
        serde_json::from_str(&render_output(&snapshot, &windows).unwrap()).unwrap();
    assert_eq!(windows_json["windowAnalyses"][0]["durationMins"], 10_080);
    assert_eq!(windows_json["windowAnalyses"][0]["partial"], false);
    assert!(
        windows_json["windowAnalyses"][0]
            .get("partialReasons")
            .is_none()
    );
    assert_eq!(
        windows_json["windowAnalyses"][0]["attribution"]["window"]["label"],
        "week"
    );
    assert_eq!(
        windows_json["windowAnalyses"][0]["threads"][0]["usage"]["localTokenSharePercent"],
        100.0
    );
    assert_eq!(
        windows_json["windowAnalyses"][0]["threads"][0]["usage"]["quotaConfidence"],
        "medium"
    );
    assert_eq!(
        windows_json["windowAnalyses"][0]["models"][0]["quotaConfidence"],
        "medium"
    );
    assert!(windows_json.get("tasks").is_none());
    assert!(windows_json.get("turns").is_none());
    assert!(windows_json.get("models").is_none());
    assert!(windows_json.get("attribution").is_none());

    let full_json: Value = serde_json::from_str(
        &render_output(
            &snapshot,
            &OutputRequest {
                sections: Section::all(),
                ..tasks.clone()
            },
        )
        .unwrap(),
    )
    .unwrap();
    assert!(full_json.get("windowAnalyses").is_some());
    assert!(full_json.get("tasks").is_some());
    assert!(full_json.get("attribution").is_some());

    let windows_text = render_output(
        &snapshot,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            ..windows.clone()
        },
    )
    .unwrap();
    assert!(windows_text.contains("week | codex | 10080m"));
    assert!(windows_text.contains(&(now - chrono::Duration::days(5)).to_rfc3339()));
    assert!(windows_text.contains(&(now + chrono::Duration::days(2)).to_rfc3339()));
    assert!(windows_text.contains("task"));
    assert!(windows_text.contains("gpt-test"));
    assert!(windows_text.contains("xhigh"));
    assert!(windows_text.contains("message preview"));
    assert!(windows_text.contains("100.00%"));
    assert!(windows_text.contains("~1.25%"));
    assert!(windows_text.contains("TOKEN%"));

    let mut partial_window = snapshot.clone();
    partial_window.window_analyses[0].partial = true;
    partial_window.window_analyses[0]
        .partial_reasons
        .push("rollout_lookback_incomplete".to_string());
    let partial_window_text = render_output(
        &partial_window,
        &OutputRequest {
            format: OutputFormat::Text,
            compact: false,
            ..windows.clone()
        },
    )
    .unwrap();
    assert!(partial_window_text.contains("[PARTIAL]"));
    assert!(partial_window_text.contains("rollout_lookback_incomplete"));

    let mut zero_call_window = snapshot.clone();
    let analysis = &mut zero_call_window.window_analyses[0];
    analysis.attribution.local_token_usage = TokenUsage::default();
    analysis.threads.clear();
    analysis.turns.clear();
    analysis.models.clear();
    assert!(!request_is_failure(&zero_call_window, &windows));
    assert!(
        render_output(
            &zero_call_window,
            &OutputRequest {
                format: OutputFormat::Text,
                compact: false,
                ..windows.clone()
            }
        )
        .unwrap()
        .contains("no token events in this reset cycle")
    );

    let mut no_windows = snapshot.clone();
    no_windows.window_analyses.clear();
    assert!(request_is_failure(&no_windows, &windows));
    assert!(
        render_output(
            &no_windows,
            &OutputRequest {
                format: OutputFormat::Text,
                compact: false,
                ..windows.clone()
            }
        )
        .unwrap()
        .contains("unavailable")
    );

    let mut empty_snapshot = snapshot.clone();
    empty_snapshot.tasks.clear();
    assert!(!request_is_partial(&empty_snapshot, &tasks));
    assert!(!request_is_failure(&empty_snapshot, &tasks));
    let empty_filtered_turns = OutputRequest {
        sections: BTreeSet::from([Section::Turns]),
        thread_filter: Some("missing-thread".to_string()),
        ..tasks.clone()
    };
    assert!(!request_is_partial(&empty_snapshot, &empty_filtered_turns));
    assert!(!request_is_failure(&empty_snapshot, &empty_filtered_turns));

    #[cfg(unix)]
    {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let mut non_unicode = snapshot.clone();
        non_unicode.codex_home =
            std::path::PathBuf::from(OsString::from_vec(b"/tmp/codex-\xff".to_vec()));
        non_unicode.tasks[0].cwd = Some(std::path::PathBuf::from(OsString::from_vec(
            b"/tmp/project-\xfe".to_vec(),
        )));
        let json: Value =
            serde_json::from_str(&render_output(&non_unicode, &tasks).unwrap()).unwrap();
        assert_eq!(json["codexHome"], "/tmp/codex-\u{fffd}");
        assert_eq!(json["tasks"][0]["cwd"], "/tmp/project-\u{fffd}");
    }

    let mut unavailable = snapshot;
    unavailable.sources[0].status = "error".to_string();
    unavailable.limits.clear();
    unavailable.rate_limit_reset_credits = None;
    unavailable.tasks.clear();
    assert!(request_is_failure(&unavailable, &tasks));
    assert!(request_is_failure(&unavailable, &limits));

    unavailable.turns.push(TurnRecord {
        thread_id: "present-thread".to_string(),
        turn_id: "turn-1".to_string(),
        model: None,
        reasoning_effort: None,
        service_tier: None,
        message_preview: None,
        started_at: None,
        completed_at: None,
        duration_ms: None,
        status: Default::default(),
        token_usage: TokenUsage::default(),
        window_token_usage: TokenUsage::default(),
        local_token_share_percent: 0.0,
        estimated_quota_percent: 0.0,
        quota_confidence: Confidence::Unknown,
        api_equivalent_cost: Default::default(),
    });
    let filtered_turns = OutputRequest {
        sections: BTreeSet::from([Section::Turns]),
        thread_filter: Some("missing-thread".to_string()),
        ..tasks.clone()
    };
    assert!(request_is_failure(&unavailable, &filtered_turns));
    let matching_turns = OutputRequest {
        thread_filter: Some("present-thread".to_string()),
        ..filtered_turns
    };
    assert!(!request_is_failure(&unavailable, &matching_turns));
}

#[test]
fn terminal_text_removes_control_characters() {
    assert_eq!(
        terminal_safe_text("before\u{1b}[2Jafter\u{7}\u{202e}done"),
        "before [2Jafter  done"
    );
}
