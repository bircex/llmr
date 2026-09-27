//! What each model costs, kept current: shipped tables, a daily sync, and prices set by hand.
//!
//! Vendors publish prices on web pages and in no API, and they change them on their own
//! schedule. Three sources, in order of precedence:
//!
//! 1. **Set by hand**, per provider and model, through the management API. For a negotiated
//!    rate, a discount, or a model the other two do not know. Always wins.
//! 2. **Synced** from a public, machine readable price list (LiteLLM's, by default) once a
//!    day. Only for a provider on its vendor's own endpoint, as the shipped tables are.
//! 3. **Shipped** with the image, in `models/*-prices.toml`. What a fresh or offline
//!    install prices with.
//!
//! A synced price that moves by more than half is held rather than applied, and waits for
//! somebody to accept it: a list somebody else maintains can be wrong, and a price that
//! drops to a third of itself is as likely a typo as a price cut. A new model's price, or a
//! smaller change, applies on the next build of the gateway. Nothing here is on the request
//! path: a sync that fails leaves the last good prices in place.
//!
//! Every priced request records which book edition priced it, so a price that changes
//! later never re-prices the past.

use crate::records::{Provider, ProviderType};
use crate::server::AppState;
use llmr::transport::{HttpRequest, HttpTransport, Reqwest};
use llmr::{Micros, PriceBook, Rate};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Where the sync reads from unless `LLMR_PRICE_SYNC_URL` says otherwise.
pub const DEFAULT_SOURCE: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

/// A synced price that moves by more than this fraction, either way, is held for a person.
const HOLD_BEYOND: f64 = 0.5;

/// The vendors whose own endpoints have published prices, as the source names them.
const VENDORS: [&str; 3] = ["anthropic", "openai", "gemini"];

/// How the sync is set up, from the environment.
#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub source: String,
    pub every: Duration,
}

impl Config {
    /// No sync, for a test or an install that says so.
    #[allow(dead_code)]
    pub fn off() -> Config {
        Config {
            enabled: false,
            source: DEFAULT_SOURCE.to_string(),
            every: Duration::from_secs(24 * 3600),
        }
    }

    /// `LLMR_PRICE_SYNC` (`off` to turn it off), `LLMR_PRICE_SYNC_URL` and
    /// `LLMR_PRICE_SYNC_HOURS` (1 to 720, default 24).
    pub fn from_env() -> Config {
        let enabled = !matches!(
            std::env::var("LLMR_PRICE_SYNC")
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
                .as_str(),
            "off" | "false" | "0" | "no"
        );
        let source = std::env::var("LLMR_PRICE_SYNC_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_SOURCE.to_string());
        let hours = std::env::var("LLMR_PRICE_SYNC_HOURS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .filter(|h| (1..=720).contains(h))
            .unwrap_or(24);
        Config {
            enabled,
            source,
            every: Duration::from_secs(hours * 3600),
        }
    }
}

/// The sync's settings, and a lock so the schedule and a request through the management API
/// never run two at once.
pub struct Sync {
    pub config: Config,
    running: tokio::sync::Mutex<()>,
}

impl Sync {
    pub fn new(config: Config) -> Sync {
        Sync {
            config,
            running: tokio::sync::Mutex::new(()),
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

/// Reads the price list once, applies what it can, holds what needs a person, and rebuilds
/// the gateway when a price that applies changed.
///
/// # Errors
///
/// When the list cannot be fetched or read, or the store cannot be written. The prices in
/// place stay in place, and the failure is kept for the management API to show.
pub async fn sync(state: &AppState) -> Result<Summary, String> {
    let _one_at_a_time = state.prices.running.lock().await;
    let source = state.prices.config.source.clone();
    let started = now();
    let mut record = state
        .db
        .run(|store| store.price_sync_state())
        .await
        .unwrap_or_default();
    record.last_attempt = Some(started);
    record.source = Some(source.clone());

    let fetched = async {
        let transport = Reqwest::new(Duration::from_secs(60)).map_err(|e| e.to_string())?;
        let response = transport
            .send(HttpRequest::get(source.clone()))
            .await
            .map_err(|e| format!("{source}: {e}"))?;
        response.check().map_err(|e| format!("{source}: {e}"))?;
        parse_source(&response.body)
    }
    .await;

    let parsed = match fetched {
        Ok(parsed) => parsed,
        Err(why) => {
            record.last_error = Some(why.clone());
            let kept = record.clone();
            let _ = state
                .db
                .run(move |store| store.set_price_sync_state(&kept))
                .await;
            return Err(why);
        }
    };

    let book = format!("synced-{}", day(started));
    let skipped = parsed.skipped;
    let (summary, changes) = state
        .db
        .run_mut(move |store| {
            let existing: BTreeMap<(String, String), SyncedPrice> = store
                .synced_prices()?
                .into_iter()
                .map(|row| ((row.vendor.clone(), row.model.clone()), row))
                .collect();
            let books: BTreeMap<&str, PriceBook> = VENDORS
                .iter()
                .filter_map(|v| Some((*v, shipped(v)?)))
                .collect();
            let from_shipped = |vendor: &str, model: &str| {
                books.get(vendor).and_then(|b| b.rates.get(model)).copied()
            };
            let (changes, mut summary) = plan(&existing, &from_shipped, &parsed.rows);
            summary.skipped = skipped;
            record.last_success = Some(started);
            record.last_error = None;
            record.last = Some(summary.clone());
            store.apply_price_changes(&changes, &book, &record)?;
            Ok((summary, changes))
        })
        .await
        .map_err(|e| e.to_string())?;

    for change in &changes {
        match change {
            Change::Apply {
                vendor,
                model,
                rate,
                was: Some(was),
            } => tracing::info!(
                vendor = %vendor,
                model = %model,
                was = %rate_view(was),
                now = %rate_view(rate),
                "price changed"
            ),
            Change::Hold {
                vendor, model, why, ..
            } => tracing::warn!(
                vendor = %vendor,
                model = %model,
                why = %why,
                "price held: accept or reject it through the management API"
            ),
            _ => {}
        }
    }
    tracing::info!(
        seen = summary.seen,
        applied = summary.applied,
        changed = summary.changed,
        held = summary.held,
        skipped = summary.skipped,
        "prices synced"
    );

    if summary.applied > 0 {
        crate::manage::reload(state).await.map_err(|e| {
            format!(
                "the prices were stored, and the gateway could not be rebuilt: {}",
                e.message
            )
        })?;
    }
    Ok(summary)
}

/// Syncs on the configured schedule for as long as the process runs.
///
/// Waits out what is left of the interval since the last success first, so a restart does
/// not fetch again. A failure tries again within the hour.
pub async fn keep_current(state: Arc<AppState>) {
    let config = state.prices.config.clone();
    if !config.enabled {
        tracing::info!(
            "price sync is off: prices come from the shipped tables and any set by hand"
        );
        return;
    }
    let every = i64::try_from(config.every.as_secs()).unwrap_or(i64::MAX);
    let last = state
        .db
        .run(|store| store.price_sync_state())
        .await
        .ok()
        .and_then(|s| s.last_success);
    if let Some(last) = last {
        let due = last.saturating_add(every).saturating_sub(now());
        if due > 0 {
            tokio::time::sleep(Duration::from_secs(u64::try_from(due).unwrap_or(0))).await;
        }
    }
    loop {
        let wait = match sync(&state).await {
            Ok(_) => config.every,
            Err(why) => {
                tracing::warn!(error = %why, "price sync failed; the prices in place stay");
                config.every.min(Duration::from_secs(3600))
            }
        };
        tokio::time::sleep(wait).await;
    }
}

/// A price the sync wrote, and one it is holding back.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncedPrice {
    pub vendor: String,
    pub model: String,
    /// What applies. `None` when the only synced price so far is held, so the shipped one
    /// still applies.
    pub rate: Option<Rate>,
    /// The edition that wrote `rate`, `litellm-YYYY-MM-DD`.
    pub book: String,
    pub updated_at: i64,
    pub held: Option<Held>,
    /// A held price somebody refused. The same price arriving again is not held again.
    pub rejected: Option<Rate>,
}

/// A synced price waiting for a person, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Held {
    pub rate: Rate,
    pub book: String,
    pub at: i64,
    pub why: String,
}

/// A price somebody set by hand, for one model on one provider.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceOverride {
    pub provider_id: String,
    pub model: String,
    pub rate: Rate,
    pub note: Option<String>,
    pub updated_at: i64,
}

impl PriceOverride {
    /// The book edition a cost priced from it names.
    pub fn book(&self) -> String {
        format!("manual-{}", day(self.updated_at))
    }
}

/// What the last sync did, kept in the store for the management API.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    pub last_attempt: Option<i64>,
    pub last_success: Option<i64>,
    pub last_error: Option<String>,
    pub source: Option<String>,
    pub last: Option<Summary>,
}

/// One sync, counted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// Rows for the three vendors the source had a usable price for.
    pub seen: usize,
    /// Written because they were new or changed.
    pub applied: usize,
    /// Of those, prices that replaced one that applied before: the ones worth logging.
    pub changed: usize,
    pub held: usize,
    pub unchanged: usize,
    /// Rows for the three vendors that could not be read as a price llmr can charge.
    pub skipped: usize,
}

/// A `YYYY-MM-DD` date from seconds since the epoch, in UTC.
pub fn day(secs: i64) -> String {
    // The civil calendar from a day number, the inverse of the one the engine dates its
    // books with. Exact for every date a clock will read.
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let d = day_of_year - (153 * shifted + 2) / 5 + 1;
    let m = if shifted < 10 {
        shifted + 3
    } else {
        shifted - 9
    };
    let y = year_of_era + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

// ----- reading the source -----

/// Rows read from the source, and how many were passed over.
#[derive(Debug, Default)]
pub struct Parsed {
    pub rows: Vec<(String, String, Rate)>,
    pub skipped: usize,
}

/// A price per unit in dollars, as micros per `scale` units. `None` for anything that is not
/// a finite, non negative number below a sanity ceiling.
fn micros(value: Option<&Value>, scale: f64) -> Option<Micros> {
    let v = value?.as_f64()?;
    if !v.is_finite() || v < 0.0 {
        return None;
    }
    let scaled = (v * scale).round();
    // A thousand dollars per million tokens, per picture or per second is past anything
    // sold today; past it is a unit mistake in the source, not a price.
    if scaled > 1_000_000_000.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    Some(Micros(scaled as i64))
}

/// Per token, as the source writes it, to per million tokens in micros.
const PER_MILLION: f64 = 1e12;
/// Per unit to micros per unit.
const PER_UNIT: f64 = 1e6;

/// What one entry of the source costs, as a [`Rate`], or why it cannot be one.
fn rate_of(entry: &Value) -> Result<Rate, &'static str> {
    let field = |name: &str| entry.get(name);
    let costs = entry.as_object().ok_or("not an object")?;
    // A model priced one way below a context size and another above it cannot be one flat
    // rate. Left unpriced, as the shipped tables leave it, rather than right for short
    // prompts and quietly wrong for long ones.
    if costs.keys().any(|k| k.contains("_above_")) {
        return Err("priced in context bands");
    }
    let token = |name: &str| micros(field(name), PER_MILLION).unwrap_or_default();
    let mut rate = Rate::default();
    match field("mode").and_then(Value::as_str).unwrap_or("chat") {
        "chat" | "responses" | "completion" | "embedding" => {
            rate.input = token("input_cost_per_token");
            rate.output = token("output_cost_per_token");
            rate.cache_read = token("cache_read_input_token_cost");
            rate.cache_write = token("cache_creation_input_token_cost");
        }
        "image_generation" => {
            // The tokens a picture is billed as, where the vendor reports them; per picture
            // where it does not. Never both, which would charge the picture twice.
            rate.input = token("input_cost_per_token");
            rate.cache_read = token("cache_read_input_token_cost");
            if field("output_cost_per_image_token").is_some() {
                rate.output = token("output_cost_per_image_token");
            } else {
                rate.image = micros(field("output_cost_per_image"), PER_UNIT).unwrap_or_default();
            }
        }
        "audio_speech" => {
            if field("input_cost_per_character").is_some() {
                rate.character =
                    micros(field("input_cost_per_character"), PER_MILLION).unwrap_or_default();
            } else {
                rate.input = token("input_cost_per_token");
                rate.output = if field("output_cost_per_audio_token").is_some() {
                    token("output_cost_per_audio_token")
                } else {
                    token("output_cost_per_token")
                };
            }
        }
        "audio_transcription" => {
            // By the token where the vendor reports tokens, by the second where it reports
            // the recording's length. One or the other, for the reason images are.
            if field("input_cost_per_audio_token").is_some() {
                rate.input = token("input_cost_per_audio_token");
                rate.output = token("output_cost_per_token");
            } else {
                rate.audio_second =
                    micros(field("input_cost_per_second"), PER_UNIT).unwrap_or_default();
            }
        }
        _ => return Err("not a kind of model llmr serves"),
    }
    if rate == Rate::default() {
        return Err("no price");
    }
    Ok(rate)
}

/// Reads the source: a JSON object of model name to entry, LiteLLM's layout.
///
/// # Errors
///
/// When it is not JSON, or not an object, or holds no price for any of the three vendors:
/// a list that suddenly prices nothing is broken, and applying it would change nothing
/// while saying all was well.
pub fn parse_source(bytes: &[u8]) -> Result<Parsed, String> {
    let document: Value =
        serde_json::from_slice(bytes).map_err(|e| format!("the price list is not JSON: {e}"))?;
    let entries = document
        .as_object()
        .ok_or("the price list is not a JSON object")?;
    let mut parsed = Parsed::default();
    for (key, entry) in entries {
        let Some(vendor) = entry.get("litellm_provider").and_then(Value::as_str) else {
            continue;
        };
        if !VENDORS.contains(&vendor) {
            continue;
        }
        // Gemini's rows are named `gemini/<model>`; the other two name the model bare, and
        // a slashed name there is some other route to the same vendor.
        let model = match vendor {
            "gemini" => match key.strip_prefix("gemini/") {
                Some(model) => model,
                None => continue,
            },
            _ if key.contains('/') => continue,
            _ => key.as_str(),
        };
        match rate_of(entry) {
            Ok(rate) => parsed
                .rows
                .push((vendor.to_string(), model.to_string(), rate)),
            Err(_) => parsed.skipped += 1,
        }
    }
    if parsed.rows.is_empty() {
        return Err("the price list has no prices for Anthropic, OpenAI or Gemini".into());
    }
    Ok(parsed)
}

// ----- deciding what to apply -----

/// What a sync does to one model's row.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Write this rate as the one that applies.
    Apply {
        vendor: String,
        model: String,
        rate: Rate,
        /// What applied before, synced or shipped, when something did.
        was: Option<Rate>,
    },
    /// Keep what applies, and hold this one for a person.
    Hold {
        vendor: String,
        model: String,
        rate: Rate,
        why: String,
    },
    /// A held price the source no longer gives: drop it.
    Unhold { vendor: String, model: String },
}

/// Why moving from `old` to `new` needs a person, if it does.
fn needs_a_person(old: &Rate, new: &Rate) -> Option<String> {
    let parts = [
        ("input", old.input, new.input),
        ("output", old.output, new.output),
        ("cache read", old.cache_read, new.cache_read),
        ("cache write", old.cache_write, new.cache_write),
        ("per picture", old.image, new.image),
        ("per second", old.audio_second, new.audio_second),
        ("per character", old.character, new.character),
    ];
    for (name, was, now) in parts {
        if was.0 == 0 {
            // A charge appearing, such as a cache write the old table did not list, is
            // the list knowing more, not a price moving.
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let ratio = now.0 as f64 / was.0 as f64;
        if now.0 == 0 || (ratio - 1.0).abs() > HOLD_BEYOND {
            return Some(format!("{name} would go from {was} to {now}"));
        }
    }
    None
}

/// What a sync should do, given what the store holds and what the source says.
///
/// `shipped` answers what the image's own table charges for a model, which is what applies
/// where nothing has been synced yet, and so what a new price is compared against.
pub fn plan(
    existing: &BTreeMap<(String, String), SyncedPrice>,
    shipped: &dyn Fn(&str, &str) -> Option<Rate>,
    fresh: &[(String, String, Rate)],
) -> (Vec<Change>, Summary) {
    let mut changes = Vec::new();
    let mut summary = Summary {
        seen: fresh.len(),
        ..Summary::default()
    };
    for (vendor, model, rate) in fresh {
        let row = existing.get(&(vendor.clone(), model.clone()));
        let applies = row.and_then(|r| r.rate).or_else(|| shipped(vendor, model));
        if applies.as_ref() == Some(rate) {
            summary.unchanged += 1;
            if row.is_some_and(|r| r.held.is_some()) {
                changes.push(Change::Unhold {
                    vendor: vendor.clone(),
                    model: model.clone(),
                });
            }
            continue;
        }
        if row.is_some_and(|r| r.rejected.as_ref() == Some(rate))
            || row.is_some_and(|r| r.held.as_ref().is_some_and(|h| h.rate == *rate))
        {
            // Refused already, or already waiting: nothing new to say.
            summary.unchanged += 1;
            continue;
        }
        match applies.as_ref().and_then(|old| needs_a_person(old, rate)) {
            Some(why) => {
                summary.held += 1;
                changes.push(Change::Hold {
                    vendor: vendor.clone(),
                    model: model.clone(),
                    rate: *rate,
                    why,
                });
            }
            None => {
                summary.applied += 1;
                if applies.is_some() {
                    summary.changed += 1;
                }
                changes.push(Change::Apply {
                    vendor: vendor.clone(),
                    model: model.clone(),
                    rate: *rate,
                    was: applies,
                });
            }
        }
    }
    (changes, summary)
}

/// The image's own table for a vendor.
pub fn shipped(vendor: &str) -> Option<PriceBook> {
    use llmr::providers::{anthropic, gemini, openai};
    match vendor {
        "anthropic" => Some(anthropic::api::shipped_prices()),
        "openai" => Some(openai::api::shipped_prices()),
        "gemini" => Some(gemini::api::shipped_prices()),
        _ => None,
    }
}

/// The vendor whose published prices hold for this provider, if any do.
///
/// Only a provider on its vendor's own endpoint: OpenAI's price for a model says nothing
/// about what Groq charges for the same name.
pub fn vendor_of(record: &Provider) -> Option<&'static str> {
    if record.base_url.is_some() {
        return None;
    }
    match record.provider_type {
        ProviderType::Anthropic => Some("anthropic"),
        ProviderType::Openai => Some("openai"),
        ProviderType::Gemini => Some("gemini"),
        ProviderType::OpenaiCompatible => None,
    }
}

// ----- composing what applies -----

/// Where the price a provider charges for a model came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Manual,
    Synced,
    Shipped,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Manual => "manual",
            Origin::Synced => "synced",
            Origin::Shipped => "shipped",
        }
    }
}

/// The prices one provider charges, from all three sources, and where each came from.
#[derive(Debug, Clone)]
pub struct Effective {
    /// One book the router and the cost read. Its id is not an edition; `origins` is.
    pub book: PriceBook,
    /// For each model, where its rate came from and the edition to record.
    pub origins: BTreeMap<String, (Origin, String)>,
}

impl Effective {
    /// Shipped, then synced, then set by hand, each overruling the one before.
    pub fn compose(
        record: &Provider,
        synced: &[SyncedPrice],
        overrides: &[PriceOverride],
    ) -> Option<Effective> {
        let vendor = vendor_of(record);
        let mut origins = BTreeMap::new();
        let mut book = match vendor.and_then(shipped) {
            Some(book) => {
                for model in book.rates.keys() {
                    origins.insert(model.clone(), (Origin::Shipped, book.id.clone()));
                }
                book
            }
            None => PriceBook::new(
                format!("{}-prices", record.id),
                record.id.clone(),
                "1970-01-01",
                "llmr",
                "1970-01-01",
                "USD",
            ),
        };
        if let Some(vendor) = vendor {
            for row in synced.iter().filter(|r| r.vendor == vendor) {
                if let Some(rate) = row.rate {
                    book.rates.insert(row.model.clone(), rate);
                    origins.insert(row.model.clone(), (Origin::Synced, row.book.clone()));
                }
            }
        }
        for row in overrides.iter().filter(|o| o.provider_id == record.id) {
            book.rates.insert(row.model.clone(), row.rate);
            origins.insert(row.model.clone(), (Origin::Manual, row.book()));
        }
        (!book.rates.is_empty()).then_some(Effective { book, origins })
    }

    /// Where a model's price came from, if it has one.
    pub fn origin(&self, model: &str) -> Option<&(Origin, String)> {
        self.origins.get(model)
    }
}

// ----- views -----

/// A rate as the management API writes it: decimal text, units named by the field.
pub fn rate_view(rate: &Rate) -> Value {
    let mut view = json!({
        "input": rate.input.exact(),
        "cache_read": rate.cache_read.exact(),
        "cache_write": rate.cache_write.exact(),
        "output": rate.output.exact(),
    });
    for (name, value) in [
        ("image", rate.image),
        ("audio_second", rate.audio_second),
        ("character", rate.character),
    ] {
        if value.0 != 0 {
            view[name] = json!(value.exact());
        }
    }
    view
}

/// A rate from the management API. Every field optional; at least one must not be zero.
///
/// # Errors
///
/// A field that is not a decimal number, a negative one, or a rate of nothing at all.
pub fn rate_from(body: &Value) -> Result<Rate, String> {
    let mut rate = Rate::default();
    let fields: [(&str, &mut Micros); 7] = [
        ("input", &mut rate.input),
        ("cache_read", &mut rate.cache_read),
        ("cache_write", &mut rate.cache_write),
        ("output", &mut rate.output),
        ("image", &mut rate.image),
        ("audio_second", &mut rate.audio_second),
        ("character", &mut rate.character),
    ];
    for (name, slot) in fields {
        match body.get(name) {
            None | Some(Value::Null) => {}
            Some(Value::String(text)) => {
                let value = Micros::parse(text).map_err(|e| format!("{name}: {e}"))?;
                if value.0 < 0 {
                    return Err(format!("{name} is negative"));
                }
                *slot = value;
            }
            Some(_) => {
                return Err(format!(
                    "{name} is decimal text, such as \"2.50\", so no float rounds it"
                ))
            }
        }
    }
    if rate == Rate::default() {
        return Err(
            "a price needs at least one of input, output, cache_read, cache_write, \
                    image, audio_second or character"
                .into(),
        );
    }
    Ok(rate)
}

/// A held price as the management API writes it.
pub fn held_view(row: &SyncedPrice) -> Option<Value> {
    let held = row.held.as_ref()?;
    Some(json!({
        "vendor": row.vendor,
        "model": row.model,
        "applies": row.rate.map(|r| rate_view(&r)),
        "held": rate_view(&held.rate),
        "book": held.book,
        "since": held.at,
        "why": held.why,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_date_is_written_from_seconds_in_utc() {
        assert_eq!(day(0), "1970-01-01");
        assert_eq!(day(1_790_517_075), "2026-09-27");
        // The leap day, and the day after it.
        assert_eq!(day(951_782_400), "2000-02-29");
        assert_eq!(day(951_868_800), "2000-03-01");
    }

    fn entry(fields: Value) -> Value {
        fields
    }

    #[test]
    fn chat_rows_are_read_per_million_tokens_to_the_micro() {
        let rate = rate_of(&entry(json!({
            "mode": "chat",
            "input_cost_per_token": 2e-06,
            "output_cost_per_token": 1e-05,
            "cache_read_input_token_cost": 2e-07,
            "cache_creation_input_token_cost": 2.5e-06,
        })))
        .unwrap_or_default();
        assert_eq!(rate.input, Micros(2_000_000));
        assert_eq!(rate.output, Micros(10_000_000));
        assert_eq!(rate.cache_read, Micros(200_000));
        assert_eq!(rate.cache_write, Micros(2_500_000));
    }

    #[test]
    fn a_model_priced_in_context_bands_is_left_unpriced() {
        assert_eq!(
            rate_of(&json!({
                "mode": "chat",
                "input_cost_per_token": 5e-06,
                "input_cost_per_token_above_272k_tokens": 1e-05,
                "output_cost_per_token": 3e-05,
            })),
            Err("priced in context bands")
        );
    }

    #[test]
    fn media_models_are_priced_by_one_unit_never_two() {
        // An image model that reports its image tokens is priced by them, and not also per
        // picture, though the list gives both.
        let image = rate_of(&json!({
            "mode": "image_generation",
            "input_cost_per_token": 3e-07,
            "output_cost_per_image": 0.039,
            "output_cost_per_image_token": 3e-05,
        }))
        .unwrap_or_default();
        assert_eq!(image.output, Micros(30_000_000));
        assert_eq!(image.image, Micros(0));

        let pictures = rate_of(&json!({
            "mode": "image_generation",
            "output_cost_per_image": 0.04,
        }))
        .unwrap_or_default();
        assert_eq!(pictures.image, Micros(40_000));

        let whisper = rate_of(&json!({
            "mode": "audio_transcription",
            "input_cost_per_second": 0.0001,
            "output_cost_per_second": 0.0001,
        }))
        .unwrap_or_default();
        assert_eq!(whisper.audio_second, Micros(100));
        assert_eq!(whisper.input, Micros(0));

        let transcribe = rate_of(&json!({
            "mode": "audio_transcription",
            "input_cost_per_audio_token": 2.5e-06,
            "input_cost_per_second": 0.0001,
            "output_cost_per_token": 1e-05,
        }))
        .unwrap_or_default();
        assert_eq!(transcribe.input, Micros(2_500_000));
        assert_eq!(transcribe.audio_second, Micros(0));

        let tts = rate_of(&json!({
            "mode": "audio_speech",
            "input_cost_per_character": 1.5e-05,
        }))
        .unwrap_or_default();
        assert_eq!(tts.character, Micros(15_000_000));
    }

    #[test]
    fn nonsense_in_the_list_is_passed_over_rather_than_priced() {
        assert!(rate_of(&json!({ "mode": "chat", "input_cost_per_token": -1.0 })).is_err());
        assert!(rate_of(&json!({ "mode": "chat", "input_cost_per_token": 5.0 })).is_err());
        assert!(rate_of(&json!({ "mode": "rerank", "input_cost_per_token": 1e-06 })).is_err());
    }

    #[test]
    fn the_source_is_read_for_the_three_vendors_under_the_names_llmr_uses() {
        let parsed = parse_source(
            json!({
                "sample_spec": { "litellm_provider": "one of the providers" },
                "claude-sonnet-5": { "litellm_provider": "anthropic", "mode": "chat",
                    "input_cost_per_token": 2e-06, "output_cost_per_token": 1e-05 },
                "gpt-5.6-sol": { "litellm_provider": "openai", "mode": "chat",
                    "input_cost_per_token": 4e-06, "output_cost_per_token": 2e-05 },
                "openai/gpt-5.6-sol": { "litellm_provider": "openai", "mode": "chat",
                    "input_cost_per_token": 9e-06, "output_cost_per_token": 9e-05 },
                "gemini/gemini-2.5-flash": { "litellm_provider": "gemini", "mode": "chat",
                    "input_cost_per_token": 3e-07, "output_cost_per_token": 2.5e-06 },
                "gemini-2.5-flash": { "litellm_provider": "vertex_ai-language-models",
                    "mode": "chat", "input_cost_per_token": 1.0, "output_cost_per_token": 1.0 },
                "bedrock/claude": { "litellm_provider": "bedrock", "mode": "chat",
                    "input_cost_per_token": 1e-06 },
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap_or_default();
        let names: Vec<(&str, &str)> = parsed
            .rows
            .iter()
            .map(|(v, m, _)| (v.as_str(), m.as_str()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("anthropic", "claude-sonnet-5"),
                ("gemini", "gemini-2.5-flash"),
                ("openai", "gpt-5.6-sol"),
            ]
        );
    }

    #[test]
    fn a_list_with_no_prices_is_refused_rather_than_applied() {
        assert!(parse_source(b"{}").is_err());
        assert!(parse_source(b"[]").is_err());
        assert!(parse_source(b"<html>").is_err());
    }

    fn tokens(input: i64, output: i64) -> Rate {
        Rate::tokens(Micros(input), Micros(0), Micros(0), Micros(output))
    }

    fn fresh(model: &str, rate: Rate) -> Vec<(String, String, Rate)> {
        vec![("anthropic".into(), model.into(), rate)]
    }

    #[test]
    fn a_small_change_applies_and_a_large_one_waits_for_a_person() {
        let shipped = |_: &str, model: &str| match model {
            "sonnet" => Some(tokens(3_000_000, 15_000_000)),
            "opus" => Some(tokens(15_000_000, 75_000_000)),
            _ => None,
        };
        let none = BTreeMap::new();

        // A third off: applied.
        let (changes, summary) = plan(
            &none,
            &shipped,
            &fresh("sonnet", tokens(2_000_000, 10_000_000)),
        );
        assert!(matches!(&changes[..], [Change::Apply { was: Some(_), .. }]));
        assert_eq!(summary.changed, 1);

        // Two thirds off: held, with the reason.
        let (changes, summary) = plan(
            &none,
            &shipped,
            &fresh("opus", tokens(5_000_000, 25_000_000)),
        );
        match &changes[..] {
            [Change::Hold { why, .. }] => {
                assert!(why.contains("input would go from 15.000000 to 5.000000"))
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(summary.held, 1);

        // A model nothing priced before: applied, it replaces no price.
        let (changes, summary) = plan(&none, &shipped, &fresh("new", tokens(1, 1)));
        assert!(matches!(&changes[..], [Change::Apply { was: None, .. }]));
        assert_eq!((summary.applied, summary.changed), (1, 0));
    }

    #[test]
    fn a_price_somebody_refused_is_not_held_again_and_a_settled_one_is_let_go() {
        let rate = tokens(5_000_000, 25_000_000);
        let mut existing = BTreeMap::new();
        existing.insert(
            ("anthropic".to_string(), "opus".to_string()),
            SyncedPrice {
                vendor: "anthropic".into(),
                model: "opus".into(),
                rate: Some(tokens(15_000_000, 75_000_000)),
                book: "litellm-2026-09-01".into(),
                updated_at: 0,
                held: None,
                rejected: Some(rate),
            },
        );
        let none = |_: &str, _: &str| None;
        let (changes, summary) = plan(&existing, &none, &fresh("opus", rate));
        assert!(changes.is_empty());
        assert_eq!(summary.unchanged, 1);

        // Held, and then the source goes back to what applies: the hold is dropped.
        let mut held = existing.clone();
        if let Some(row) = held.values_mut().next() {
            row.rejected = None;
            row.held = Some(Held {
                rate,
                book: "litellm-2026-09-02".into(),
                at: 0,
                why: "w".into(),
            });
        }
        let (changes, _) = plan(&held, &none, &fresh("opus", tokens(15_000_000, 75_000_000)));
        assert!(matches!(&changes[..], [Change::Unhold { .. }]));
    }

    #[test]
    fn a_charge_the_old_table_did_not_list_is_not_a_price_moving() {
        let old = tokens(4_000_000, 20_000_000);
        let mut new = old;
        new.cache_write = Micros(5_000_000);
        assert_eq!(needs_a_person(&old, &new), None);
        new.output = Micros(0);
        assert!(needs_a_person(&old, &new).is_some());
    }

    fn provider(id: &str, kind: ProviderType, base_url: Option<&str>) -> Provider {
        Provider::for_test(id, kind, base_url)
    }

    #[test]
    fn set_by_hand_beats_synced_beats_shipped_and_each_names_its_edition() {
        let record = provider("anthropic", ProviderType::Anthropic, None);
        let synced = vec![SyncedPrice {
            vendor: "anthropic".into(),
            model: "claude-haiku-4-5".into(),
            rate: Some(tokens(900_000, 4_500_000)),
            book: "litellm-2026-09-27".into(),
            updated_at: 0,
            held: None,
            rejected: None,
        }];
        let manual = vec![PriceOverride {
            provider_id: "anthropic".into(),
            model: "claude-sonnet-5".into(),
            rate: tokens(1_000_000, 5_000_000),
            note: None,
            updated_at: 1_790_517_075,
        }];
        let effective = Effective::compose(&record, &synced, &manual).map(|e| e.origins);
        let origins = effective.unwrap_or_default();
        assert_eq!(
            origins.get("claude-opus-5").map(|o| o.0),
            Some(Origin::Shipped)
        );
        assert_eq!(
            origins.get("claude-haiku-4-5").cloned(),
            Some((Origin::Synced, "litellm-2026-09-27".to_string()))
        );
        assert_eq!(
            origins.get("claude-sonnet-5").cloned(),
            Some((Origin::Manual, "manual-2026-09-27".to_string()))
        );
    }

    #[test]
    fn a_vendor_price_never_reaches_another_endpoint_but_a_hand_set_one_does() {
        let groq = provider("groq", ProviderType::OpenaiCompatible, Some("https://groq"));
        let proxy = provider("proxy", ProviderType::Anthropic, Some("https://proxy"));
        let synced = vec![SyncedPrice {
            vendor: "openai".into(),
            model: "gpt".into(),
            rate: Some(tokens(1, 1)),
            book: "b".into(),
            updated_at: 0,
            held: None,
            rejected: None,
        }];
        assert!(Effective::compose(&groq, &synced, &[]).is_none());
        assert!(Effective::compose(&proxy, &synced, &[]).is_none());
        let manual = vec![PriceOverride {
            provider_id: "groq".into(),
            model: "llama".into(),
            rate: tokens(100_000, 100_000),
            note: None,
            updated_at: 0,
        }];
        assert!(Effective::compose(&groq, &synced, &manual)
            .is_some_and(|e| e.origin("llama").is_some()));
    }

    #[test]
    fn a_rate_from_the_api_is_decimal_text_and_is_something() {
        let rate = rate_from(&json!({ "input": "2.50", "output": "10" })).unwrap_or_default();
        assert_eq!(rate.input, Micros(2_500_000));
        assert!(rate_from(&json!({ "input": 2.5 })).is_err());
        assert!(rate_from(&json!({ "input": "-1" })).is_err());
        assert!(rate_from(&json!({})).is_err());
        assert!(rate_from(&json!({ "image": "0.04" })).is_ok());
    }
}
