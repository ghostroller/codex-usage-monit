//! Reconcile source-owned request records with their legacy counter mirrors.
use super::*;

#[derive(Default)]
struct TurnRequests<'a> {
    requests: HashMap<&'a str, TokenUsage>,
    sum: TokenUsage,
    covered_through: Option<usize>,
    inconsistent: bool,
}

pub(super) fn request_covered_counters(
    events: &[ParsedEvent],
    as_of: Option<DateTime<Utc>>,
) -> HashSet<usize> {
    let mut turns = HashMap::<&str, TurnRequests<'_>>::new();
    let mut counters = Vec::new();
    let mut current_turn = None;
    let mut pending_request = None;
    let mut covered = HashSet::new();
    for (index, event) in events.iter().enumerate() {
        if as_of.is_some_and(|now| parsed_event_available_at(event).is_some_and(|at| at > now)) {
            pending_request = None;
            continue;
        }
        match event {
            ParsedEvent::TaskStarted { turn_id, .. } => {
                current_turn = Some(turn_id.as_str());
                pending_request = None;
            }
            ParsedEvent::TurnContext { payload, .. } => {
                if let Some(turn) = string_field_in(payload, &["turn_id", "turnId"]) {
                    if current_turn != Some(turn) {
                        pending_request = None;
                    }
                    current_turn = Some(turn);
                }
            }
            ParsedEvent::RequestUsage {
                turn_id,
                response_id,
                usage,
                turn_usage,
                ..
            } => {
                let state = turns.entry(turn_id).or_default();
                match state.requests.insert(response_id, *usage) {
                    Some(previous) if previous != *usage => state.inconsistent = true,
                    Some(_) => {}
                    None => state.sum.add_assign(*usage),
                }
                if let Some(total) = turn_usage {
                    if *total == state.sum && !state.inconsistent {
                        state.covered_through = Some(index);
                    } else {
                        // A native suffix does not prove coverage of an earlier
                        // legacy-only prefix or an inherited cumulative gap.
                        state.inconsistent = true;
                    }
                }
                pending_request = Some((turn_id.as_str(), *usage));
            }
            ParsedEvent::TokenCount {
                total_usage,
                last_usage,
                ..
            } => {
                if let Some(turn) = current_turn {
                    counters.push((index, turn));
                    // Consume only one following mirror, retaining its counter
                    // baseline so later legacy-only deltas remain countable.
                    if let Some((request_turn, usage)) = pending_request.take()
                        && request_turn == turn
                        && *last_usage == Some(usage)
                        && total_usage.is_some_and(|total| {
                            total.has_valid_breakdown() && total.total_tokens >= usage.total_tokens
                        })
                    {
                        covered.insert(index);
                    }
                }
            }
            _ => {}
        }
    }
    for (index, turn) in counters {
        if let Some(state) = turns.get(turn)
            && !state.inconsistent
            && state.covered_through.is_some_and(|end| index <= end)
        {
            covered.insert(index);
        }
    }
    covered
}

pub(super) fn deduplicate_native_calls(
    dataset: &mut RolloutDataset,
    threads: &HashMap<String, ThreadBuilder>,
) {
    // Deduplication runs after all selected files have been replayed, including
    // overlapping copies. It also runs on every as-of materialization.
    let mut identities = HashMap::<String, (TokenUsage, Option<String>)>::new();
    let mut conflicts = HashSet::new();
    for call in &dataset.calls {
        let Some(id) = call
            .usage_event_id
            .as_ref()
            .filter(|id| id.starts_with(NATIVE_USAGE_EVENT_ID_PREFIX))
        else {
            continue;
        };
        let value = (call.tokens, call.turn_id.clone());
        if identities
            .insert(id.clone(), value.clone())
            .is_some_and(|previous| previous != value)
        {
            conflicts.insert(id.clone());
        }
    }
    for id in &conflicts {
        push_replay_warning(
            dataset,
            format!("conflicting native request identity {id}; excluded its ambiguous usage"),
        );
        dataset.stats.ambiguous_token_resets += 1;
    }
    let mut seen = HashSet::new();
    dataset.calls.retain(|call| {
        let Some(id) = call
            .usage_event_id
            .as_ref()
            .filter(|id| id.starts_with(NATIVE_USAGE_EVENT_ID_PREFIX))
        else {
            return true;
        };
        !conflicts.contains(id) && seen.insert(id.clone())
    });
    for call in &mut dataset.calls {
        if call
            .usage_event_id
            .as_ref()
            .is_some_and(|id| id.starts_with(NATIVE_USAGE_EVENT_ID_PREFIX))
            && let Some(turn) = threads
                .get(&call.thread_id)
                .and_then(|thread| call.turn_id.as_ref().and_then(|id| thread.turns.get(id)))
        {
            if call.model.is_none() {
                call.model = turn.model.clone();
            }
            if call.service_tier.is_none() {
                call.service_tier = turn.service_tier.clone();
            }
        }
    }
}
