use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use sha2::{Digest, Sha256};

pub(crate) const MODEL_CATALOG_VERSION: u32 = 1;
pub(crate) const BUNDLED_ESTIMATOR_REVISION: u32 = 6;
pub(crate) const BUNDLED_API_PRICING_CATALOG_REVISION: u32 = 3;
pub(crate) const BUNDLED_API_PRICING_RATES_AS_OF: &str = "2026-09-07";
pub(crate) const BUNDLED_API_PRICING_SOURCE_URL: &str =
    "https://developers.openai.com/api/docs/pricing";

const MODEL_CATALOG_FILE: &str = "model-catalog.json";
const MAX_CATALOG_BYTES: u64 = 1_048_576;
const MAX_MODELS: usize = 256;
const MAX_ALIASES_PER_MODEL: usize = 64;
const MAX_MODEL_NAME_BYTES: usize = 128;
const MAX_METADATA_BYTES: usize = 2_048;
const CREDIT_UNITS_PER_CREDIT: u128 = 8;
const MICRO_USD_PER_USD: u128 = 1_000_000;
// A configured rate can participate in up to four API components, or three
// credit components whose Longx multipliers total 5.5x. Reserving six full
// u64 token components guarantees that every per-call multiplication and
// component sum remains representable in u128 without relying on saturation.
const MAX_CONFIGURED_RATE_UNITS: u128 = u128::MAX / (u64::MAX as u128) / 6;
const CATALOG_FINGERPRINT_DOMAIN: &[u8] = b"codex-usage-monit/model-catalog/v1\0";
pub(crate) const MODEL_CATALOG_FINGERPRINT_PREFIX: &str = "model-catalog-sha256-v1-";

static ACTIVE_CATALOG: OnceLock<CatalogState> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CreditTokenRates {
    pub(crate) input: u128,
    pub(crate) cached_input: u128,
    pub(crate) output: u128,
}

impl CreditTokenRates {
    const fn new(input: u128, cached_input: u128, output: u128) -> Self {
        Self {
            input,
            cached_input,
            output,
        }
    }

    pub(crate) fn long_context(self) -> Self {
        Self {
            input: self.input.saturating_mul(2),
            cached_input: self.cached_input.saturating_mul(2),
            output: self.output.saturating_mul(3) / 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CreditModelRates {
    pub(crate) standard: CreditTokenRates,
    pub(crate) fast: Option<CreditTokenRates>,
    pub(crate) long_context_pricing: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApiTokenRates {
    pub(crate) input: u128,
    pub(crate) cached_input: u128,
    pub(crate) cache_write: Option<u128>,
    pub(crate) output: u128,
}

impl ApiTokenRates {
    const fn new(input: u128, cached_input: u128, cache_write: Option<u128>, output: u128) -> Self {
        Self {
            input,
            cached_input,
            cache_write,
            output,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApiLongContextRates {
    Published(ApiTokenRates),
    Flat,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApiTierRates {
    pub(crate) short: ApiTokenRates,
    pub(crate) long: ApiLongContextRates,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ApiModelRates {
    pub(crate) standard: ApiTierRates,
    pub(crate) fast: Option<ApiTierRates>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ModelEntry {
    credit: Option<CreditModelRates>,
    api: Option<ApiModelRates>,
}

#[derive(Debug)]
struct ModelCatalog {
    estimator_revision: u32,
    api_pricing_catalog_revision: u32,
    rates_as_of: String,
    source_url: String,
    long_context_input_threshold: u64,
    credit_fallback_model: String,
    models: BTreeMap<String, ModelEntry>,
}

#[derive(Debug)]
struct CatalogState {
    catalog: ModelCatalog,
    fingerprint: String,
    configured_path: Option<PathBuf>,
    external: bool,
    error: Option<(io::ErrorKind, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogStatus {
    pub(crate) configured_path: Option<PathBuf>,
    pub(crate) external: bool,
    pub(crate) estimator_revision: u32,
    pub(crate) api_pricing_catalog_revision: u32,
    pub(crate) fingerprint: String,
}

pub(crate) fn initialize(path_override: Option<PathBuf>) -> io::Result<CatalogStatus> {
    let state = ACTIVE_CATALOG.get_or_init(|| {
        if cfg!(test) {
            let catalog = bundled_catalog();
            CatalogState {
                fingerprint: catalog_fingerprint(&catalog),
                catalog,
                configured_path: path_override,
                external: false,
                error: None,
            }
        } else {
            discover_catalog(path_override)
        }
    });
    if let Some((kind, message)) = &state.error {
        return Err(io::Error::new(*kind, message.clone()));
    }
    Ok(CatalogStatus {
        configured_path: state.configured_path.clone(),
        external: state.external,
        estimator_revision: state.catalog.estimator_revision,
        api_pricing_catalog_revision: state.catalog.api_pricing_catalog_revision,
        fingerprint: state.fingerprint.clone(),
    })
}

pub(crate) fn credit_rates(model: Option<&str>, fast: bool) -> Option<CreditTokenRates> {
    let rates = lookup(model)?.credit?;
    if fast {
        rates.fast
    } else {
        Some(rates.standard)
    }
}

pub(crate) fn fallback_credit_rates(fast: bool) -> CreditTokenRates {
    let catalog = active_catalog();
    let rates = catalog
        .models
        .get(&catalog.credit_fallback_model)
        .and_then(|entry| entry.credit)
        .expect("validated model catalog credit fallback is available");
    if fast {
        rates
            .fast
            .expect("validated model catalog fast credit fallback is available")
    } else {
        rates.standard
    }
}

pub(crate) fn model_supports_credit_long_context(model: Option<&str>) -> bool {
    lookup(model)
        .and_then(|entry| entry.credit)
        .is_some_and(|rates| rates.long_context_pricing)
}

pub(crate) fn api_model_rates(model: Option<&str>) -> Option<ApiModelRates> {
    lookup(model)?.api
}

pub(crate) fn long_context_input_threshold() -> u64 {
    active_catalog().long_context_input_threshold
}

pub(crate) fn estimator_revision() -> u32 {
    active_catalog().estimator_revision
}

pub(crate) fn api_pricing_catalog_revision() -> u32 {
    active_catalog().api_pricing_catalog_revision
}

pub(crate) fn api_pricing_rates_as_of() -> &'static str {
    active_catalog().rates_as_of.as_str()
}

pub(crate) fn api_pricing_source_url() -> &'static str {
    active_catalog().source_url.as_str()
}

pub(crate) fn model_catalog_fingerprint() -> &'static str {
    active_state().fingerprint.as_str()
}

fn lookup(model: Option<&str>) -> Option<ModelEntry> {
    let normalized = normalize_model_name(model?);
    if normalized.is_empty() {
        return None;
    }
    active_catalog().models.get(&normalized).copied()
}

fn active_catalog() -> &'static ModelCatalog {
    &active_state().catalog
}

fn active_state() -> &'static CatalogState {
    ACTIVE_CATALOG.get_or_init(|| {
        if cfg!(test) {
            let catalog = bundled_catalog();
            CatalogState {
                fingerprint: catalog_fingerprint(&catalog),
                catalog,
                configured_path: None,
                external: false,
                error: None,
            }
        } else {
            discover_catalog(None)
        }
    })
}

fn discover_catalog(path_override: Option<PathBuf>) -> CatalogState {
    let configured_path = configured_catalog_path(path_override);
    let Some(path) = configured_path.clone() else {
        let catalog = bundled_catalog();
        return CatalogState {
            fingerprint: catalog_fingerprint(&catalog),
            catalog,
            configured_path: None,
            external: false,
            error: None,
        };
    };

    match load_catalog(&path) {
        Ok(Some(catalog)) => {
            let fingerprint = catalog_fingerprint(&catalog);
            CatalogState {
                catalog,
                fingerprint,
                configured_path: Some(path),
                external: true,
                error: None,
            }
        }
        Ok(None) => {
            let catalog = bundled_catalog();
            CatalogState {
                fingerprint: catalog_fingerprint(&catalog),
                catalog,
                configured_path: Some(path),
                external: false,
                error: None,
            }
        }
        Err(error) => {
            let catalog = bundled_catalog();
            CatalogState {
                fingerprint: catalog_fingerprint(&catalog),
                catalog,
                configured_path: Some(path.clone()),
                external: false,
                error: Some((
                    error.kind(),
                    format!("unable to load model catalog {}: {error}", path.display()),
                )),
            }
        }
    }
}

pub(crate) fn configured_catalog_path(path_override: Option<PathBuf>) -> Option<PathBuf> {
    if path_override.is_some() {
        return path_override;
    }
    crate::open_config::default_open_config_path()
        .and_then(|path| path.parent().map(|parent| parent.join(MODEL_CATALOG_FILE)))
}

pub(crate) fn catalog_path_beside(config_file: &Path) -> Option<PathBuf> {
    config_file
        .parent()
        .map(|parent| parent.join(MODEL_CATALOG_FILE))
}

fn load_catalog(path: &Path) -> io::Result<Option<ModelCatalog>> {
    // Distinguish a genuinely absent path from an existing dangling link.
    // The opened-handle checks below remain authoritative if the entry is
    // replaced between this probe and `open`.
    match fs::symlink_metadata(path) {
        // Windows rejects an attempt to open a directory through OpenOptions
        // before we can inspect the opened handle. Reject known non-files at
        // the metadata probe as well, while retaining the handle checks below
        // as the authority against replacement races.
        Ok(metadata)
            if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_file() =>
        {
            return Err(invalid_config(
                "model catalog path must be a regular non-link file",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }

    let mut options = OpenOptions::new();
    options.read(true);
    add_nofollow_flags(&mut options);
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(map_nofollow_error(error)),
    };
    let metadata = file.metadata()?;
    if metadata_is_link_or_reparse(&metadata) || !metadata.file_type().is_file() {
        return Err(invalid_config(
            "model catalog path must be a regular non-link file",
        ));
    }
    if metadata.len() > MAX_CATALOG_BYTES {
        return Err(invalid_config(format!(
            "model catalog exceeds the {MAX_CATALOG_BYTES}-byte limit"
        )));
    }
    let capacity = usize::try_from(metadata.len().min(MAX_CATALOG_BYTES)).unwrap_or(0);
    let mut contents = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(MAX_CATALOG_BYTES.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).unwrap_or(u64::MAX) > MAX_CATALOG_BYTES {
        return Err(invalid_config(format!(
            "model catalog exceeds the {MAX_CATALOG_BYTES}-byte limit"
        )));
    }
    parse_catalog(&contents).map(Some)
}

fn add_nofollow_flags(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        // O_NOFOLLOW closes the final-component link race. O_NONBLOCK keeps a
        // FIFO or device swapped into place from blocking before fstat rejects
        // the opened handle as non-regular.
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

fn map_nofollow_error(error: io::Error) -> io::Error {
    #[cfg(unix)]
    if error.raw_os_error() == Some(libc::ELOOP) {
        return invalid_config("model catalog path must not be a symbolic link");
    }
    error
}

#[cfg(unix)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(any(unix, windows)))]
fn metadata_is_link_or_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn parse_catalog(contents: &[u8]) -> io::Result<ModelCatalog> {
    let config: CatalogConfig = serde_json::from_slice(contents)
        .map_err(|error| invalid_config(format!("invalid model catalog JSON: {error}")))?;
    ModelCatalog::from_config(config)
}

impl ModelCatalog {
    fn from_config(config: CatalogConfig) -> io::Result<Self> {
        if config.version != MODEL_CATALOG_VERSION {
            return Err(invalid_config(format!(
                "unsupported model catalog version {}; expected {}",
                config.version, MODEL_CATALOG_VERSION
            )));
        }
        if config.estimator_revision <= BUNDLED_ESTIMATOR_REVISION {
            return Err(invalid_config(format!(
                "estimatorRevision must be greater than the bundled revision {BUNDLED_ESTIMATOR_REVISION}"
            )));
        }
        if config.api_pricing_catalog_revision <= BUNDLED_API_PRICING_CATALOG_REVISION {
            return Err(invalid_config(format!(
                "apiPricingCatalogRevision must be greater than the bundled revision {BUNDLED_API_PRICING_CATALOG_REVISION}"
            )));
        }
        validate_metadata("ratesAsOf", &config.rates_as_of)?;
        validate_metadata("sourceUrl", &config.source_url)?;
        if config.long_context_input_threshold == 0 {
            return Err(invalid_config(
                "longContextInputThreshold must be greater than zero",
            ));
        }
        validate_model_name("creditFallbackModel", &config.credit_fallback_model)?;
        if config.models.is_empty() || config.models.len() > MAX_MODELS {
            return Err(invalid_config(format!(
                "models must contain between 1 and {MAX_MODELS} entries"
            )));
        }

        let mut catalog = Self {
            estimator_revision: config.estimator_revision,
            api_pricing_catalog_revision: config.api_pricing_catalog_revision,
            rates_as_of: config.rates_as_of,
            source_url: config.source_url,
            long_context_input_threshold: config.long_context_input_threshold,
            credit_fallback_model: normalize_model_name(&config.credit_fallback_model),
            models: BTreeMap::new(),
        };
        for model in config.models {
            catalog.insert_configured(model)?;
        }
        let fallback = catalog
            .models
            .get(&catalog.credit_fallback_model)
            .and_then(|entry| entry.credit)
            .ok_or_else(|| {
                invalid_config("creditFallbackModel must reference a model with credit rates")
            })?;
        if fallback.fast.is_none() {
            return Err(invalid_config(
                "creditFallbackModel must define both Standard and Fast credit rates",
            ));
        }
        Ok(catalog)
    }

    fn insert_configured(&mut self, model: ModelConfig) -> io::Result<()> {
        validate_model_name("model id", &model.id)?;
        if model.aliases.len() > MAX_ALIASES_PER_MODEL {
            return Err(invalid_config(format!(
                "model {} has more than {MAX_ALIASES_PER_MODEL} aliases",
                model.id
            )));
        }
        if model.credit.is_none() && model.api.is_none() {
            return Err(invalid_config(format!(
                "model {} must define credit, api, or both",
                model.id
            )));
        }

        let entry = ModelEntry {
            credit: model
                .credit
                .map(CreditModelRates::from_config)
                .transpose()?,
            api: model.api.map(ApiModelRates::from_config).transpose()?,
        };
        self.insert_name(&model.id, entry)?;
        for alias in model.aliases {
            validate_model_name("model alias", &alias)?;
            self.insert_name(&alias, entry)?;
        }
        Ok(())
    }

    fn insert_name(&mut self, name: &str, entry: ModelEntry) -> io::Result<()> {
        let normalized = normalize_model_name(name);
        if self.models.insert(normalized.clone(), entry).is_some() {
            return Err(invalid_config(format!(
                "duplicate model id or alias {normalized:?}"
            )));
        }
        Ok(())
    }

    fn insert_builtin(&mut self, names: &[&str], entry: ModelEntry) {
        for name in names {
            let previous = self.models.insert(normalize_model_name(name), entry);
            debug_assert!(previous.is_none(), "duplicate bundled model name {name}");
        }
    }
}

impl CreditModelRates {
    fn from_config(config: CreditModelConfig) -> io::Result<Self> {
        let rates = Self {
            standard: CreditTokenRates::from_config(config.standard, "credit.standard")?,
            fast: config
                .fast
                .map(|rates| CreditTokenRates::from_config(rates, "credit.fast"))
                .transpose()?,
            long_context_pricing: config.long_context_pricing,
        };
        if rates.long_context_pricing
            && (!rates.standard.output.is_multiple_of(2)
                || rates
                    .fast
                    .is_some_and(|fast| !fast.output.is_multiple_of(2)))
        {
            return Err(invalid_config(
                "credit long-context output rates must support an exact 1.5x multiplier",
            ));
        }
        Ok(rates)
    }
}

impl CreditTokenRates {
    fn from_config(config: CreditTokenRatesConfig, field: &str) -> io::Result<Self> {
        let rates = Self {
            input: parse_scaled_decimal(
                &config.input,
                CREDIT_UNITS_PER_CREDIT,
                &format!("{field}.input"),
            )?,
            cached_input: parse_scaled_decimal(
                &config.cached_input,
                CREDIT_UNITS_PER_CREDIT,
                &format!("{field}.cachedInput"),
            )?,
            output: parse_scaled_decimal(
                &config.output,
                CREDIT_UNITS_PER_CREDIT,
                &format!("{field}.output"),
            )?,
        };
        validate_required_rates(field, rates.input, rates.cached_input, rates.output)?;
        validate_rate_bounds(
            field,
            &[
                ("input", rates.input),
                ("cachedInput", rates.cached_input),
                ("output", rates.output),
            ],
        )?;
        Ok(rates)
    }
}

impl ApiModelRates {
    fn from_config(config: ApiModelConfig) -> io::Result<Self> {
        Ok(Self {
            standard: ApiTierRates::from_config(config.standard, "api.standard")?,
            fast: config
                .fast
                .map(|rates| ApiTierRates::from_config(rates, "api.fast"))
                .transpose()?,
        })
    }
}

impl ApiTierRates {
    fn from_config(config: ApiTierConfig, field: &str) -> io::Result<Self> {
        let short = ApiTokenRates::from_config(config.short, &format!("{field}.short"))?;
        let long = match config.long {
            ApiLongContextConfig::Published { rates } => ApiLongContextRates::Published(
                ApiTokenRates::from_config(rates, &format!("{field}.long.rates"))?,
            ),
            ApiLongContextConfig::Flat => ApiLongContextRates::Flat,
            ApiLongContextConfig::Unavailable => ApiLongContextRates::Unavailable,
        };
        Ok(Self { short, long })
    }
}

impl ApiTokenRates {
    fn from_config(config: ApiTokenRatesConfig, field: &str) -> io::Result<Self> {
        let rates = Self {
            input: parse_scaled_decimal(
                &config.input,
                MICRO_USD_PER_USD,
                &format!("{field}.input"),
            )?,
            cached_input: parse_scaled_decimal(
                &config.cached_input,
                MICRO_USD_PER_USD,
                &format!("{field}.cachedInput"),
            )?,
            cache_write: config
                .cache_write
                .as_ref()
                .map(|value| {
                    parse_scaled_decimal(value, MICRO_USD_PER_USD, &format!("{field}.cacheWrite"))
                })
                .transpose()?,
            output: parse_scaled_decimal(
                &config.output,
                MICRO_USD_PER_USD,
                &format!("{field}.output"),
            )?,
        };
        validate_required_rates(field, rates.input, rates.cached_input, rates.output)?;
        validate_rate_bounds(
            field,
            &[
                ("input", rates.input),
                ("cachedInput", rates.cached_input),
                ("cacheWrite", rates.cache_write.unwrap_or_default()),
                ("output", rates.output),
            ],
        )?;
        Ok(rates)
    }
}

fn validate_required_rates(
    field: &str,
    input: u128,
    _cached_input: u128,
    output: u128,
) -> io::Result<()> {
    if input == 0 || output == 0 {
        Err(invalid_config(format!(
            "{field} input and output rates must be greater than zero"
        )))
    } else {
        Ok(())
    }
}

fn validate_rate_bounds(field: &str, rates: &[(&str, u128)]) -> io::Result<()> {
    if let Some((component, _)) = rates
        .iter()
        .find(|(_, rate)| *rate > MAX_CONFIGURED_RATE_UNITS)
    {
        return Err(invalid_config(format!(
            "{field}.{component} exceeds the maximum safe configured rate"
        )));
    }
    Ok(())
}

fn parse_scaled_decimal(value: &DecimalValue, scale: u128, field: &str) -> io::Result<u128> {
    let raw = value.to_plain_string();
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with('-') || raw.starts_with('+') {
        return Err(invalid_config(format!(
            "{field} must be a non-negative plain decimal"
        )));
    }
    let mut pieces = raw.split('.');
    let whole = pieces.next().unwrap_or_default();
    let fraction = pieces.next();
    if pieces.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction
            .is_some_and(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(invalid_config(format!(
            "{field} must be a non-negative plain decimal"
        )));
    }
    let whole = whole
        .parse::<u128>()
        .map_err(|_| invalid_config(format!("{field} is too large")))?;
    let (fraction, denominator) = if let Some(fraction) = fraction {
        let denominator = 10_u128
            .checked_pow(u32::try_from(fraction.len()).unwrap_or(u32::MAX))
            .ok_or_else(|| invalid_config(format!("{field} has too many decimal places")))?;
        let fraction = fraction
            .parse::<u128>()
            .map_err(|_| invalid_config(format!("{field} is too large")))?;
        (fraction, denominator)
    } else {
        (0, 1)
    };
    let numerator = whole
        .checked_mul(denominator)
        .and_then(|value| value.checked_add(fraction))
        .and_then(|value| value.checked_mul(scale))
        .ok_or_else(|| invalid_config(format!("{field} is too large")))?;
    if numerator % denominator != 0 {
        return Err(invalid_config(format!(
            "{field} cannot be represented exactly at the supported precision"
        )));
    }
    Ok(numerator / denominator)
}

fn validate_model_name(field: &str, value: &str) -> io::Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_MODEL_NAME_BYTES {
        return Err(invalid_config(format!(
            "{field} must contain 1 to {MAX_MODEL_NAME_BYTES} bytes"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(invalid_config(format!(
            "{field} must not contain control characters"
        )));
    }
    Ok(())
}

fn validate_metadata(field: &str, value: &str) -> io::Result<()> {
    if value.trim().is_empty() || value.len() > MAX_METADATA_BYTES {
        Err(invalid_config(format!(
            "{field} must contain 1 to {MAX_METADATA_BYTES} bytes"
        )))
    } else if value.chars().any(char::is_control) {
        Err(invalid_config(format!(
            "{field} must not contain control characters"
        )))
    } else {
        Ok(())
    }
}

fn normalize_model_name(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn catalog_fingerprint(catalog: &ModelCatalog) -> String {
    let mut digest = Sha256::new();
    digest.update(CATALOG_FINGERPRINT_DOMAIN);
    fingerprint_u32(&mut digest, b"catalog-version", MODEL_CATALOG_VERSION);
    fingerprint_u32(
        &mut digest,
        b"estimator-revision",
        catalog.estimator_revision,
    );
    fingerprint_u32(
        &mut digest,
        b"api-pricing-catalog-revision",
        catalog.api_pricing_catalog_revision,
    );
    fingerprint_bytes(&mut digest, b"rates-as-of", catalog.rates_as_of.as_bytes());
    fingerprint_bytes(&mut digest, b"source-url", catalog.source_url.as_bytes());
    fingerprint_u64(
        &mut digest,
        b"long-context-input-threshold",
        catalog.long_context_input_threshold,
    );
    fingerprint_bytes(
        &mut digest,
        b"credit-fallback-model",
        catalog.credit_fallback_model.as_bytes(),
    );
    fingerprint_u64(
        &mut digest,
        b"model-name-count",
        u64::try_from(catalog.models.len()).expect("bounded catalog length fits in u64"),
    );
    for (name, entry) in &catalog.models {
        fingerprint_bytes(&mut digest, b"model-name", name.as_bytes());
        fingerprint_credit_model(&mut digest, entry.credit);
        fingerprint_api_model(&mut digest, entry.api);
    }
    format!("{MODEL_CATALOG_FINGERPRINT_PREFIX}{:x}", digest.finalize())
}

fn fingerprint_credit_model(digest: &mut Sha256, rates: Option<CreditModelRates>) {
    fingerprint_bool(digest, b"credit-present", rates.is_some());
    let Some(rates) = rates else {
        return;
    };
    fingerprint_credit_rates(digest, b"credit-standard", rates.standard);
    fingerprint_bool(digest, b"credit-fast-present", rates.fast.is_some());
    if let Some(fast) = rates.fast {
        fingerprint_credit_rates(digest, b"credit-fast", fast);
    }
    fingerprint_bool(
        digest,
        b"credit-long-context-pricing",
        rates.long_context_pricing,
    );
}

fn fingerprint_credit_rates(digest: &mut Sha256, label: &[u8], rates: CreditTokenRates) {
    fingerprint_bytes(digest, label, &[]);
    fingerprint_u128(digest, b"credit-input", rates.input);
    fingerprint_u128(digest, b"credit-cached-input", rates.cached_input);
    fingerprint_u128(digest, b"credit-output", rates.output);
}

fn fingerprint_api_model(digest: &mut Sha256, rates: Option<ApiModelRates>) {
    fingerprint_bool(digest, b"api-present", rates.is_some());
    let Some(rates) = rates else {
        return;
    };
    fingerprint_api_tier(digest, b"api-standard", rates.standard);
    fingerprint_bool(digest, b"api-fast-present", rates.fast.is_some());
    if let Some(fast) = rates.fast {
        fingerprint_api_tier(digest, b"api-fast", fast);
    }
}

fn fingerprint_api_tier(digest: &mut Sha256, label: &[u8], rates: ApiTierRates) {
    fingerprint_bytes(digest, label, &[]);
    fingerprint_api_rates(digest, b"api-short", rates.short);
    match rates.long {
        ApiLongContextRates::Published(long) => {
            fingerprint_bytes(digest, b"api-long-mode", b"published");
            fingerprint_api_rates(digest, b"api-long", long);
        }
        ApiLongContextRates::Flat => fingerprint_bytes(digest, b"api-long-mode", b"flat"),
        ApiLongContextRates::Unavailable => {
            fingerprint_bytes(digest, b"api-long-mode", b"unavailable");
        }
    }
}

fn fingerprint_api_rates(digest: &mut Sha256, label: &[u8], rates: ApiTokenRates) {
    fingerprint_bytes(digest, label, &[]);
    fingerprint_u128(digest, b"api-input", rates.input);
    fingerprint_u128(digest, b"api-cached-input", rates.cached_input);
    fingerprint_bool(
        digest,
        b"api-cache-write-present",
        rates.cache_write.is_some(),
    );
    if let Some(cache_write) = rates.cache_write {
        fingerprint_u128(digest, b"api-cache-write", cache_write);
    }
    fingerprint_u128(digest, b"api-output", rates.output);
}

fn fingerprint_bool(digest: &mut Sha256, label: &[u8], value: bool) {
    fingerprint_bytes(digest, label, &[u8::from(value)]);
}

fn fingerprint_u32(digest: &mut Sha256, label: &[u8], value: u32) {
    fingerprint_bytes(digest, label, &value.to_be_bytes());
}

fn fingerprint_u64(digest: &mut Sha256, label: &[u8], value: u64) {
    fingerprint_bytes(digest, label, &value.to_be_bytes());
}

fn fingerprint_u128(digest: &mut Sha256, label: &[u8], value: u128) {
    fingerprint_bytes(digest, label, &value.to_be_bytes());
}

fn fingerprint_bytes(digest: &mut Sha256, label: &[u8], value: &[u8]) {
    digest.update(
        u64::try_from(label.len())
            .expect("fingerprint labels fit in u64")
            .to_be_bytes(),
    );
    digest.update(label);
    digest.update(
        u64::try_from(value.len())
            .expect("bounded catalog fields fit in u64")
            .to_be_bytes(),
    );
    digest.update(value);
}

fn invalid_config(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CatalogConfig {
    version: u32,
    estimator_revision: u32,
    api_pricing_catalog_revision: u32,
    rates_as_of: String,
    source_url: String,
    long_context_input_threshold: u64,
    credit_fallback_model: String,
    models: Vec<ModelConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelConfig {
    id: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    credit: Option<CreditModelConfig>,
    #[serde(default)]
    api: Option<ApiModelConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreditModelConfig {
    standard: CreditTokenRatesConfig,
    #[serde(default)]
    fast: Option<CreditTokenRatesConfig>,
    #[serde(default)]
    long_context_pricing: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreditTokenRatesConfig {
    input: DecimalValue,
    cached_input: DecimalValue,
    output: DecimalValue,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiModelConfig {
    standard: ApiTierConfig,
    #[serde(default)]
    fast: Option<ApiTierConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiTierConfig {
    short: ApiTokenRatesConfig,
    long: ApiLongContextConfig,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
enum ApiLongContextConfig {
    Published { rates: ApiTokenRatesConfig },
    Flat,
    Unavailable,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApiTokenRatesConfig {
    input: DecimalValue,
    cached_input: DecimalValue,
    #[serde(default)]
    cache_write: Option<DecimalValue>,
    output: DecimalValue,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum DecimalValue {
    String(String),
    Number(serde_json::Number),
}

impl DecimalValue {
    fn to_plain_string(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Number(value) => value.to_string(),
        }
    }
}

fn credit_model(
    standard: CreditTokenRates,
    fast: Option<CreditTokenRates>,
    long_context_pricing: bool,
) -> Option<CreditModelRates> {
    Some(CreditModelRates {
        standard,
        fast,
        long_context_pricing,
    })
}

const fn api_tier(short: ApiTokenRates, long: ApiLongContextRates) -> ApiTierRates {
    ApiTierRates { short, long }
}

const fn luna_api_rates() -> ApiModelRates {
    ApiModelRates {
        standard: api_tier(
            ApiTokenRates::new(200_000, 20_000, Some(250_000), 1_200_000),
            ApiLongContextRates::Published(ApiTokenRates::new(
                400_000,
                40_000,
                Some(500_000),
                1_800_000,
            )),
        ),
        fast: Some(api_tier(
            ApiTokenRates::new(400_000, 40_000, Some(500_000), 2_400_000),
            ApiLongContextRates::Published(ApiTokenRates::new(
                800_000,
                80_000,
                Some(1_000_000),
                3_600_000,
            )),
        )),
    }
}

fn bundled_catalog() -> ModelCatalog {
    let mut catalog = ModelCatalog {
        estimator_revision: BUNDLED_ESTIMATOR_REVISION,
        api_pricing_catalog_revision: BUNDLED_API_PRICING_CATALOG_REVISION,
        rates_as_of: BUNDLED_API_PRICING_RATES_AS_OF.to_string(),
        source_url: BUNDLED_API_PRICING_SOURCE_URL.to_string(),
        long_context_input_threshold: 272_000,
        credit_fallback_model: "gpt-5.6-luna".to_string(),
        models: BTreeMap::new(),
    };

    catalog.insert_builtin(
        &["gpt-6-astra"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(2_000, 200, 10_000),
                Some(CreditTokenRates::new(5_000, 500, 25_000)),
                true,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(10_000_000, 1_000_000, Some(12_500_000), 50_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        20_000_000,
                        2_000_000,
                        Some(25_000_000),
                        75_000_000,
                    )),
                ),
                fast: Some(api_tier(
                    ApiTokenRates::new(20_000_000, 2_000_000, Some(25_000_000), 100_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        40_000_000,
                        4_000_000,
                        Some(50_000_000),
                        150_000_000,
                    )),
                )),
            }),
        },
    );
    catalog.insert_builtin(
        &[
            "gpt-5.6-sol",
            "gpt-5.6",
            "daybreak-blue-latest",
            "gpt-daybreak-blue-latest",
        ],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(800, 80, 4_000),
                Some(CreditTokenRates::new(2_000, 200, 10_000)),
                true,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(4_000_000, 400_000, Some(5_000_000), 20_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        8_000_000,
                        800_000,
                        Some(10_000_000),
                        30_000_000,
                    )),
                ),
                fast: Some(api_tier(
                    ApiTokenRates::new(8_000_000, 800_000, Some(10_000_000), 40_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        16_000_000,
                        1_600_000,
                        Some(20_000_000),
                        60_000_000,
                    )),
                )),
            }),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.6-terra"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(400, 40, 2_400),
                Some(CreditTokenRates::new(1_000, 100, 6_000)),
                true,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(2_000_000, 200_000, Some(2_500_000), 12_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        4_000_000,
                        400_000,
                        Some(5_000_000),
                        18_000_000,
                    )),
                ),
                fast: Some(api_tier(
                    ApiTokenRates::new(4_000_000, 400_000, Some(5_000_000), 24_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        8_000_000,
                        800_000,
                        Some(10_000_000),
                        36_000_000,
                    )),
                )),
            }),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.6-luna"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(40, 4, 240),
                Some(CreditTokenRates::new(100, 10, 600)),
                true,
            ),
            api: Some(luna_api_rates()),
        },
    );
    catalog.insert_builtin(
        &["codex-auto-review"],
        ModelEntry {
            credit: None,
            api: Some(luna_api_rates()),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.5"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(1_000, 100, 6_000),
                Some(CreditTokenRates::new(2_500, 250, 15_000)),
                true,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(5_000_000, 500_000, None, 30_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        10_000_000, 1_000_000, None, 45_000_000,
                    )),
                ),
                fast: Some(api_tier(
                    ApiTokenRates::new(12_500_000, 1_250_000, None, 75_000_000),
                    ApiLongContextRates::Unavailable,
                )),
            }),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.4"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(500, 50, 3_000),
                Some(CreditTokenRates::new(1_000, 100, 6_000)),
                true,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(2_500_000, 250_000, None, 15_000_000),
                    ApiLongContextRates::Published(ApiTokenRates::new(
                        5_000_000, 500_000, None, 22_500_000,
                    )),
                ),
                fast: Some(api_tier(
                    ApiTokenRates::new(5_000_000, 500_000, None, 30_000_000),
                    ApiLongContextRates::Unavailable,
                )),
            }),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.4-mini"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(150, 15, 904),
                Some(CreditTokenRates::new(300, 30, 1_808)),
                false,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(750_000, 75_000, None, 4_500_000),
                    ApiLongContextRates::Flat,
                ),
                fast: Some(api_tier(
                    ApiTokenRates::new(1_500_000, 150_000, None, 9_000_000),
                    ApiLongContextRates::Flat,
                )),
            }),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.3-codex", "gpt-5.2"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(350, 35, 2_800),
                Some(CreditTokenRates::new(350, 35, 2_800)),
                false,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(1_750_000, 175_000, None, 14_000_000),
                    ApiLongContextRates::Flat,
                ),
                fast: Some(api_tier(
                    ApiTokenRates::new(3_500_000, 350_000, None, 28_000_000),
                    ApiLongContextRates::Flat,
                )),
            }),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.2-codex"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(350, 35, 2_800),
                Some(CreditTokenRates::new(350, 35, 2_800)),
                false,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(1_750_000, 175_000, None, 14_000_000),
                    ApiLongContextRates::Flat,
                ),
                fast: None,
            }),
        },
    );
    catalog.insert_builtin(
        &[
            "gpt-5.6-cyber",
            "daybreak-red-latest",
            "gpt-daybreak-red-latest",
        ],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(2_500, 250, 15_000),
                Some(CreditTokenRates::new(6_250, 625, 37_500)),
                true,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(12_500_000, 1_250_000, Some(15_625_000), 75_000_000),
                    ApiLongContextRates::Unavailable,
                ),
                fast: None,
            }),
        },
    );
    catalog.insert_builtin(
        &["gpt-5.5-cyber"],
        ModelEntry {
            credit: credit_model(
                CreditTokenRates::new(2_500, 250, 15_000),
                Some(CreditTokenRates::new(6_250, 625, 37_500)),
                false,
            ),
            api: Some(ApiModelRates {
                standard: api_tier(
                    ApiTokenRates::new(12_500_000, 1_250_000, None, 75_000_000),
                    ApiLongContextRates::Unavailable,
                ),
                fast: None,
            }),
        },
    );
    catalog
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decimal(value: u128) -> DecimalValue {
        DecimalValue::String(value.to_string())
    }

    #[test]
    fn bundled_catalog_contains_astra_credit_and_api_rates() {
        let catalog = bundled_catalog();
        let astra = catalog.models.get("gpt-6-astra").unwrap();
        let credit = astra.credit.unwrap();
        assert_eq!(credit.standard, CreditTokenRates::new(2_000, 200, 10_000));
        assert_eq!(credit.fast, Some(CreditTokenRates::new(5_000, 500, 25_000)));
        let api = astra.api.unwrap();
        assert_eq!(api.standard.short.input, 10_000_000);
        assert_eq!(api.standard.short.cached_input, 1_000_000);
        assert_eq!(api.standard.short.cache_write, Some(12_500_000));
        assert_eq!(api.standard.short.output, 50_000_000);
    }

    #[test]
    fn auto_review_is_api_only_and_uses_luna_api_rates() {
        let catalog = bundled_catalog();
        let auto_review = catalog.models.get("codex-auto-review").unwrap();
        let luna = catalog.models.get("gpt-5.6-luna").unwrap();

        assert_eq!(auto_review.credit, None);
        assert_eq!(auto_review.api, luna.api);
        assert!(luna.credit.is_some());
    }

    #[test]
    fn configured_catalog_parses_decimal_rates_and_aliases() {
        let catalog = parse_catalog(
            br#"{
              "version": 1,
              "estimatorRevision": 7,
              "apiPricingCatalogRevision": 4,
              "ratesAsOf": "2026-09-07",
              "sourceUrl": "https://example.invalid/rates",
              "longContextInputThreshold": 123,
              "creditFallbackModel": "new-model",
              "models": [{
                "id": "new-model",
                "aliases": ["new-model-latest"],
                "credit": {
                  "standard": {"input": "31.25", "cachedInput": "3.125", "output": 100},
                  "fast": {"input": "62.5", "cachedInput": "6.25", "output": 200},
                  "longContextPricing": true
                },
                "api": {
                  "standard": {
                    "short": {"input": "1.25", "cachedInput": "0.125", "cacheWrite": "1.5625", "output": 10},
                    "long": {"mode": "flat"}
                  }
                }
              }]
            }"#,
        )
        .unwrap();

        assert_eq!(catalog.estimator_revision, 7);
        assert_eq!(catalog.api_pricing_catalog_revision, 4);
        assert_eq!(catalog.long_context_input_threshold, 123);
        let entry = catalog.models.get("new-model-latest").unwrap();
        assert_eq!(
            entry.credit.unwrap().standard,
            CreditTokenRates::new(250, 25, 800)
        );
        assert_eq!(
            entry.api.unwrap().standard.short.cache_write,
            Some(1_562_500)
        );
    }

    #[test]
    fn catalog_fingerprint_is_canonical_and_changes_with_pricing_content() {
        fn configured(input: &str, aliases: &str) -> ModelCatalog {
            parse_catalog(
                format!(
                    r#"{{
                      "version": 1,
                      "estimatorRevision": 7,
                      "apiPricingCatalogRevision": 4,
                      "ratesAsOf": "2026-09-07",
                      "sourceUrl": "https://example.invalid/rates",
                      "longContextInputThreshold": 272000,
                      "creditFallbackModel": "fallback",
                      "models": [{{
                        "id": "fallback",
                        "credit": {{
                          "standard": {{"input": 1, "cachedInput": 1, "output": 2}},
                          "fast": {{"input": 2, "cachedInput": 2, "output": 4}}
                        }}
                      }}, {{
                        "id": "api-only",
                        "aliases": {aliases},
                        "api": {{
                          "standard": {{
                            "short": {{"input": {input}, "cachedInput": 0, "output": 2}},
                            "long": {{"mode": "flat"}}
                          }}
                        }}
                      }}]
                    }}"#
                )
                .as_bytes(),
            )
            .unwrap()
        }

        let first = configured("1", r#"["alias-b", "alias-a"]"#);
        let reordered = configured("1.0", r#"["ALIAS-A", "ALIAS-B"]"#);
        let changed_rate = configured("1.5", r#"["alias-a", "alias-b"]"#);
        let first_fingerprint = catalog_fingerprint(&first);

        assert!(first_fingerprint.starts_with(MODEL_CATALOG_FINGERPRINT_PREFIX));
        assert_eq!(
            first_fingerprint.len(),
            MODEL_CATALOG_FINGERPRINT_PREFIX.len() + 64
        );
        assert_eq!(first_fingerprint, catalog_fingerprint(&reordered));
        assert_ne!(first_fingerprint, catalog_fingerprint(&changed_rate));
    }

    #[test]
    fn documented_complete_catalog_is_valid_and_contains_current_aliases() {
        let catalog = parse_catalog(include_bytes!("../docs/model-catalog.example.json")).unwrap();

        assert!(catalog.models.contains_key("gpt-6-astra"));
        assert_eq!(
            catalog.models.get("gpt-daybreak-blue-latest").unwrap().api,
            catalog.models.get("gpt-5.6-sol").unwrap().api
        );
        assert_eq!(
            catalog
                .models
                .get("gpt-daybreak-red-latest")
                .unwrap()
                .credit,
            catalog.models.get("gpt-5.6-cyber").unwrap().credit
        );
        let auto_review = catalog.models.get("codex-auto-review").unwrap();
        assert_eq!(auto_review.credit, None);
        assert_eq!(
            auto_review.api,
            catalog.models.get("gpt-5.6-luna").unwrap().api
        );
    }

    #[test]
    fn documented_complete_catalog_matches_bundled_catalog_semantics() {
        let documented =
            parse_catalog(include_bytes!("../docs/model-catalog.example.json")).unwrap();
        let bundled = bundled_catalog();

        // The example intentionally carries newer numeric revisions so it is
        // accepted as an override. Everything that affects lookup, pricing,
        // Longx behavior, or provenance must otherwise remain identical.
        assert_eq!(documented.rates_as_of, bundled.rates_as_of);
        assert_eq!(documented.source_url, bundled.source_url);
        assert_eq!(
            documented.long_context_input_threshold,
            bundled.long_context_input_threshold
        );
        assert_eq!(
            documented.credit_fallback_model,
            bundled.credit_fallback_model
        );
        assert_eq!(documented.models, bundled.models);
    }

    #[test]
    fn configured_catalog_rejects_duplicate_aliases_and_stale_revisions() {
        let duplicate = br#"{
          "version": 1,
          "estimatorRevision": 7,
          "apiPricingCatalogRevision": 4,
          "ratesAsOf": "2026-09-07",
          "sourceUrl": "https://example.invalid/rates",
          "longContextInputThreshold": 272000,
          "creditFallbackModel": "a",
          "models": [
            {"id":"a","aliases":["same"],"credit":{"standard":{"input":1,"cachedInput":1,"output":1},"fast":{"input":1,"cachedInput":1,"output":1}}},
            {"id":"b","aliases":["SAME"],"credit":{"standard":{"input":1,"cachedInput":1,"output":1},"fast":{"input":1,"cachedInput":1,"output":1}}}
          ]
        }"#;
        assert!(
            parse_catalog(duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );

        let stale = br#"{
          "version": 1,
          "estimatorRevision": 6,
          "apiPricingCatalogRevision": 3,
          "ratesAsOf": "2026-09-07",
          "sourceUrl": "https://example.invalid/rates",
          "longContextInputThreshold": 272000,
          "creditFallbackModel": "a",
          "models": [{"id":"a","credit":{"standard":{"input":1,"cachedInput":1,"output":1},"fast":{"input":1,"cachedInput":1,"output":1}}}]
        }"#;
        assert!(
            parse_catalog(stale)
                .unwrap_err()
                .to_string()
                .contains("greater than")
        );
    }

    #[test]
    fn configured_rate_limit_keeps_runtime_products_and_sums_representable() {
        let max_tokens = u128::from(u64::MAX);
        let even_rate = MAX_CONFIGURED_RATE_UNITS & !1;
        let long_credit = CreditTokenRates::new(
            MAX_CONFIGURED_RATE_UNITS,
            MAX_CONFIGURED_RATE_UNITS,
            even_rate,
        )
        .long_context();
        let checked_sum = |rates: &[u128]| {
            rates.iter().try_fold(0_u128, |total, rate| {
                max_tokens
                    .checked_mul(*rate)
                    .and_then(|component| total.checked_add(component))
            })
        };
        assert!(
            checked_sum(&[
                long_credit.input,
                long_credit.cached_input,
                long_credit.output,
            ])
            .is_some()
        );
        assert!(checked_sum(&[MAX_CONFIGURED_RATE_UNITS; 4]).is_some());

        let safe_credit = MAX_CONFIGURED_RATE_UNITS / CREDIT_UNITS_PER_CREDIT;
        CreditTokenRates::from_config(
            CreditTokenRatesConfig {
                input: decimal(safe_credit),
                cached_input: decimal(0),
                output: decimal(1),
            },
            "credit.standard",
        )
        .unwrap();
        let unsafe_credit = safe_credit + 1;
        let error = CreditTokenRates::from_config(
            CreditTokenRatesConfig {
                input: decimal(unsafe_credit),
                cached_input: decimal(0),
                output: decimal(1),
            },
            "credit.standard",
        )
        .unwrap_err();
        assert!(error.to_string().contains("maximum safe configured rate"));

        let unsafe_api = MAX_CONFIGURED_RATE_UNITS / MICRO_USD_PER_USD + 1;
        let error = ApiTokenRates::from_config(
            ApiTokenRatesConfig {
                input: decimal(1),
                cached_input: decimal(0),
                cache_write: Some(decimal(unsafe_api)),
                output: decimal(1),
            },
            "api.standard.short",
        )
        .unwrap_err();
        assert!(error.to_string().contains("api.standard.short.cacheWrite"));
    }

    #[test]
    fn catalog_loader_rejects_non_regular_and_oversized_files() {
        let directory = tempfile::tempdir().unwrap();
        let non_regular = load_catalog(directory.path()).unwrap_err();
        assert!(non_regular.to_string().contains("regular non-link file"));

        let oversized = directory.path().join("oversized.json");
        fs::write(
            &oversized,
            vec![b' '; usize::try_from(MAX_CATALOG_BYTES + 1).unwrap()],
        )
        .unwrap();
        let error = load_catalog(&oversized).unwrap_err();
        assert!(error.to_string().contains("exceeds"));
    }

    #[cfg(unix)]
    #[test]
    fn catalog_loader_rejects_symbolic_links_without_following_them() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let target = directory.path().join("target.json");
        fs::write(
            &target,
            include_bytes!("../docs/model-catalog.example.json"),
        )
        .unwrap();
        let link = directory.path().join("model-catalog.json");
        symlink(&target, &link).unwrap();

        let error = load_catalog(&link).unwrap_err();
        assert!(error.to_string().contains("non-link file"));

        fs::remove_file(&target).unwrap();
        let dangling_error = load_catalog(&link).unwrap_err();
        assert!(dangling_error.to_string().contains("non-link file"));
    }
}
