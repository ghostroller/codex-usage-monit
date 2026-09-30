# Model catalog maintenance

When a model launches or pricing changes, an agent should retrieve current
sources, compare their rules with the bundled catalog, and update the repository
with reviewable evidence. This workflow changes code and documentation; applying
an update to an installed program remains a separate operation.

A reusable request is: “Update model mappings according to
`docs/model-catalog-maintenance.md` using current official sources; record the
evidence, update code and tests, and do not deploy or update installed binaries.”

## Authoritative sources

Read these current documents before changing a mapping:

| Source | Evidence to extract |
| --- | --- |
| [API pricing Markdown](https://developers.openai.com/api/docs/pricing.md) | USD per million tokens: Standard/Fast, short/long context, cached input, cache write, and output. |
| [Codex pricing Markdown](https://learn.chatgpt.com/docs/pricing.md) | Subscription credit rates per million tokens and plan-specific qualifications. |
| [Speed](https://learn.chatgpt.com/docs/agent-configuration/speed) | Fast availability and the separate included-usage and purchased-credit multipliers. |
| [Codex models](https://learn.chatgpt.com/docs/models) and [API models](https://developers.openai.com/api/docs/models) | Exact model IDs, availability, supported tiers, and model-specific context limits. |

[App-server `model/list`](https://learn.chatgpt.com/docs/app-server#list-models-modellist)
and [`codex debug models`](https://learn.chatgpt.com/docs/developer-commands#codex-debug-models)
can reveal newly available model IDs. As checked on September 30, 2026, their
model JSON did not provide the complete credit and API pricing rules. Recheck
that capability if the protocol changes; a tier's UI description such as
“2x speed” is not a billing multiplier.

## Repeatable agent workflow

1. Retrieve the current documents above, including relevant model-specific pages.
   Record retrieval date, exact URLs, table rows, units, and supporting notes in
   the change description or verification record. Saved source snapshots or
   hashes can make later comparison reproducible. A page fetched through another
   tool is useful evidence; do not claim a direct HTTP fetch succeeded unless it
   did. If a source cannot be obtained or its rules conflict, record the gap.
2. Compare the evidence with `bundled_catalog()` in
   [`src/model_catalog.rs`](../src/model_catalog.rs) and the
   [complete example](model-catalog.example.json). Identify new exact IDs,
   changed rates, tier availability, context thresholds, aliases, and removed
   public rows before editing. Missing rows do not establish zero cost or justify
   deleting legacy rollout compatibility.
3. Update the bundled mappings directly using the confirmed rules below. Preserve
   unsupported or unresolved cases as explicit partial/unpriced results. Do not
   copy another model's prices solely because its name looks similar.
4. Increase the affected domain's revision, synchronize the complete example,
   extend the relevant regressions, and update current-model documentation.
   Review the resulting diff against the retrieved evidence.
5. Run the affected local checks and record their scope and result. Follow
   [`testing.md`](testing.md) for platform coverage and integration/release gates.
   Report what changed, the verified pricing basis, and any remaining unknowns.

### Pricing audit on October 1, 2026

The two pricing Markdown sources and supporting model/Speed pages were retrieved
for this update. GPT-6.1 Sol receives an independent exact-ID profile; other
currently listed bundled prices were checked and retained. GPT-5.4/mini credit
weights and their earlier `2x` Fast multiplier remain legacy compatibility
values; the current credit card and Speed page do not re-confirm them. Its full API and credit
matrices are recorded in the [data-capabilities reference](codex-data-capabilities.md).
Bundled revisions are estimator `8` and API catalog `5`; example revisions are
`9` and `6` so it remains a valid complete external override.

This audit does not establish complete support for every public model or tier.
Rosalind API billing begins on October 5, 2026; effective-date handling and
confirmed Fast/long-context semantics remain required before adding that profile.
Image pricing needs modality evidence that ordinary rollout token calls lack.
Astra Ultrafast needs a distinct tier in the schema and runtime; it is deferred
and has no supported API pricing projection in this batch.

## Rules to preserve

- **Separate API prices from Codex credits.** API Fast prices, included
  subscription Fast usage, and separately purchased-credit Fast usage are distinct
  definitions. For example, included and purchased Fast can use `2.5x` and `2x`;
  establish the applicable model and plan from Speed rather than generalizing.
  Keep the estimator's included-subscription credit basis explicit.
- **Verify IDs, aliases, and legacy support.** Model lookup is trim-normalized,
  case-insensitive exact matching. Add an alias only with evidence; preserve
  historical IDs and existing Daybreak mappings unless evidence warrants a
  change. A new Luna release does not establish a new `codex-auto-review` proxy
  or unknown-model credit fallback.
- **Keep API long-context semantics explicit.** Distinguish published long rates,
  a confirmed flat price, and unavailable long pricing. A `-` or missing long row
  must not be fabricated from another model or a generic multiplier. Check
  model-page notes against the price table and record conflicts. Apply published
  long pricing only at the verified per-request input boundary; ambiguous
  request boundaries retain ranges or partial coverage.
- **Keep Longx an optional assumption.** The credit `longContextPricing` flag
  enables the existing optional API-long-context proxy. It does not declare an
  official Codex subscription surcharge. Verify eligibility before enabling it
  for a new model; retain the default-off projection and independent API costs.
- **Keep exact arithmetic and coverage.** Credit rates use integer eighth-credit
  units; API rates use integer micro-USD per million tokens. Check new decimals
  are exactly representable rather than rounding or introducing floats. Cache
  write and reasoning are token subsets, so preserve the component formulas.
  Unknown models retain explicit credit fallback/partial flags and stay unpriced
  for API coverage.
- **Version derived meanings.** Increase estimator revision when credit mappings
  or rules change and API catalog revision when API mappings or prices change.
  Update the verified date and sources. Do not bump schema/history versions for a
  routine rate change. Remote nodes must agree on both revisions and the
  normalized catalog fingerprint; incompatible derived history stays explicit.

## Files to update

| File | Responsibility |
| --- | --- |
| [`src/model_catalog.rs`](../src/model_catalog.rs) | `bundled_catalog()`, verified IDs/aliases, credit/API rates, tier/context support, bundled revision constants and metadata, and catalog tests. |
| [`docs/model-catalog.example.json`](model-catalog.example.json) | Same mapping, rate, fallback, threshold, and metadata semantics as the bundled catalog; both revisions strictly above the bundled values. |
| [`src/attribution.rs`](../src/attribution.rs) | Credit Standard/Fast matrix, exact decimals, Longx eligibility and request-boundary regressions. |
| [`src/api_cost.rs`](../src/api_cost.rs) | API Standard/Fast short/long/cache-write matrix, unknown pricing, and boundary/range regressions. |
| [`tests/model_catalog_config.rs`](../tests/model_catalog_config.rs) | Fresh-process configuration behavior and bundled metadata expectations. |
| Both README files; [`codex-data-capabilities.md`](codex-data-capabilities.md), [`requirements.md`](requirements.md), [`implementation-plan.md`](implementation-plan.md), [`existing-tools.md`](existing-tools.md) | Current model list, prices, sources, dates, revisions, and changed assumptions. |

History and API owners already reference the bundled revision constants. Parser
or TUI production changes are needed only if observed log format or behavior
changes. The test `documented_complete_catalog_matches_bundled_catalog_semantics`
requires the example and bundled data to remain coherent.

## Focused verification and runtime effects

Read `.agent/environment.local.md`, if present, and [`testing.md`](testing.md)
before tests. For a code mapping change, these existing tests cover the affected
areas; run from the repository root on the native host:

```sh
cargo test --locked --lib model_catalog::tests::
cargo test --locked --lib attribution::tests::
cargo test --locked --lib api_cost::tests::
cargo test --locked --test model_catalog_config
```

These are focused checks, not a full platform pass. Add regressions for new
behavior, use the documented runners where platform behavior changes, and follow
the existing checkpoint policy. Documentation-only edits need link and
instruction checks, with no Rust rebuild.

Normal mapping maintenance does not deploy code or update installed binaries.
When a release or installation update is requested, use the existing release
and application-update workflow after the relevant verification gates.

An external `model-catalog.json` is a complete override loaded at startup. After
a reviewed runtime edit, restart recorder/TUI and use matching revisions and
fingerprints on every synchronized host. Both external revisions must exceed
those in the installed binary; a later bundled revision increase can invalidate
an older override, so review its migration during an application upgrade.
Existing aggregated history cannot always be repriced without retained rollout
evidence. See [catalog overrides](../README.md#model-catalog-overrides) and
[application updates](remote-updates.md) for deployment and recovery details.
