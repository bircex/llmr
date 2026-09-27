//! What every request consumed and cost, recorded without slowing it down.
//!
//! One row per request the client API answered, refused or failed: which name was asked for,
//! which route answered, the token counts, the cost, the latency and the outcome. Never the
//! prompt and never the reply.
//!
//! Rows go through a bounded channel to one writer task that inserts them in batches, so a
//! request never waits on the database. If the channel is full the row is dropped and said
//! so in the log: losing a usage row is better than stalling traffic behind a slow disk.
//!
//! # What a cost is
//!
//! Five answers, and none of them is a zero standing in for "nobody knows":
//!
//! | `cost_status` | |
//! |---|---|
//! | `priced` | The provider's published rate times the usage it reported |
//! | `partial` | Priced, but some usage fields were not reported, so it is a floor |
//! | `unpriced` | A paid provider with no known rate for the model, or no usage reported |
//! | `free` | A self hosted model: tokens are counted and nothing is charged |
//! | `subscription` | A command line tool signed in with a subscription: tokens are counted and the plan covers them |
//!
//! A request that failed before any provider answered has no cost at all.

use crate::store::Db;
use llmr::{Micros, Usage, UsageCoverage};
use rusqlite::ToSql;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// How a request ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Answered.
    Ok,
    /// The model declined.
    Refused,
    /// No answer: every route failed, none could serve it, or the request was refused.
    Error,
    /// A stream that broke, or ended before the model said it was done. What arrived was
    /// delivered and counted.
    Interrupted,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Refused => "refused",
            Outcome::Error => "error",
            Outcome::Interrupted => "interrupted",
        }
    }

    pub fn parse(text: &str) -> Outcome {
        match text {
            "ok" => Outcome::Ok,
            "refused" => Outcome::Refused,
            "interrupted" => Outcome::Interrupted,
            _ => Outcome::Error,
        }
    }
}

/// Token counts as the provider reported them. `None` is "not reported", never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Tokens {
    pub input: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub output: Option<i64>,
}

fn count(value: Option<u64>) -> Option<i64> {
    value.map(|v| i64::try_from(v).unwrap_or(i64::MAX))
}

fn add(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.saturating_add(b)),
        (a, None) => a,
        (None, b) => b,
    }
}

impl Tokens {
    pub fn from_usage(usage: &Usage) -> Tokens {
        Tokens {
            input: count(usage.input_tokens),
            cache_read: count(usage.cache_read_tokens),
            cache_write: count(usage.cache_write_tokens),
            output: count(usage.output_tokens),
        }
    }

    fn absorb(&mut self, other: Tokens) {
        self.input = add(self.input, other.input);
        self.cache_read = add(self.cache_read, other.cache_read);
        self.cache_write = add(self.cache_write, other.cache_write);
        self.output = add(self.output, other.output);
    }

    fn view(&self) -> Value {
        // The whole prompt, cached parts included, as the OpenAI shape counts it.
        let prompt = [self.input, self.cache_read, self.cache_write]
            .into_iter()
            .fold(None, add);
        json!({
            "input": self.input,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "output": self.output,
            "total": add(prompt, self.output),
        })
    }
}

/// What a request cost. See the module header for what each answer means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cost {
    Priced {
        micros: i64,
        currency: String,
        /// Some usage fields were not reported, so the amount is a floor.
        partial: bool,
    },
    Unpriced,
    Free,
    /// Covered by a subscription's flat fee: nothing is charged per call.
    Subscription,
    /// No provider answered, so there is nothing to cost.
    None,
}

impl Cost {
    /// A priced amount from the engine's price book.
    pub fn priced(amount: Micros, currency: String, coverage: UsageCoverage) -> Cost {
        Cost::Priced {
            micros: amount.0,
            currency,
            partial: coverage != UsageCoverage::Exact,
        }
    }

    pub fn status(&self) -> Option<&'static str> {
        match self {
            Cost::Priced { partial: false, .. } => Some("priced"),
            Cost::Priced { partial: true, .. } => Some("partial"),
            Cost::Unpriced => Some("unpriced"),
            Cost::Free => Some("free"),
            Cost::Subscription => Some("subscription"),
            Cost::None => None,
        }
    }

    pub fn micros(&self) -> Option<i64> {
        match self {
            Cost::Priced { micros, .. } => Some(*micros),
            _ => None,
        }
    }

    pub fn currency(&self) -> Option<&str> {
        match self {
            Cost::Priced { currency, .. } => Some(currency),
            _ => None,
        }
    }

    pub fn from_row(status: Option<&str>, micros: Option<i64>, currency: Option<String>) -> Cost {
        match (status, micros, currency) {
            (Some(s @ ("priced" | "partial")), Some(micros), Some(currency)) => Cost::Priced {
                micros,
                currency,
                partial: s == "partial",
            },
            (Some("free"), ..) => Cost::Free,
            (Some("subscription"), ..) => Cost::Subscription,
            (Some(_), ..) => Cost::Unpriced,
            (None, ..) => Cost::None,
        }
    }

    /// The cost as a reply and the management API write it.
    pub fn view(&self) -> Value {
        match self {
            Cost::Priced {
                micros,
                currency,
                partial,
            } => json!({
                "status": if *partial { "partial" } else { "priced" },
                "amount": Micros(*micros).exact(),
                "currency": currency,
            }),
            Cost::None => Value::Null,
            other => json!({ "status": other.status() }),
        }
    }

    /// The `x-llmr-cost` header value, when there is an amount to put in it.
    pub fn header(&self) -> Option<String> {
        match self {
            Cost::Priced {
                micros, currency, ..
            } => Some(format!("{} {currency}", Micros(*micros).exact())),
            _ => None,
        }
    }
}

/// One request, as recorded.
#[derive(Debug, Clone)]
pub struct UsageRecord {
    /// Unix seconds.
    pub at: i64,
    /// The id the reply carried, so a client's log and this one can be joined.
    pub request_id: String,
    /// What the client put in `model`.
    pub asked: String,
    /// The `provider/model` that answered, when one did.
    pub route: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    /// The model the provider said served it, which can be a dated alias of `model`.
    pub served_model: Option<String>,
    pub stream: bool,
    pub outcome: Outcome,
    pub error_code: Option<String>,
    pub stop_reason: Option<String>,
    pub attempts: i64,
    pub fell_through: i64,
    pub latency_ms: i64,
    pub tokens: Tokens,
    pub cost: Cost,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl UsageRecord {
    /// A request that got no answer from any provider.
    pub fn failed(
        request_id: &str,
        asked: &str,
        stream: bool,
        code: &str,
        latency_ms: i64,
    ) -> Self {
        UsageRecord {
            at: now(),
            request_id: request_id.to_string(),
            asked: asked.to_string(),
            route: None,
            provider_id: None,
            model: None,
            served_model: None,
            stream,
            outcome: Outcome::Error,
            error_code: Some(code.to_string()),
            stop_reason: None,
            attempts: 0,
            fell_through: 0,
            latency_ms,
            tokens: Tokens::default(),
            cost: Cost::None,
        }
    }

    pub fn view(&self, id: i64) -> Value {
        json!({
            "id": id,
            "at": self.at,
            "request_id": self.request_id,
            "asked": self.asked,
            "route": self.route,
            "provider": self.provider_id,
            "model": self.model,
            "served_model": self.served_model,
            "stream": self.stream,
            "outcome": self.outcome.as_str(),
            "error_code": self.error_code,
            "stop_reason": self.stop_reason,
            "attempts": self.attempts,
            "fell_through": self.fell_through,
            "latency_ms": self.latency_ms,
            "tokens": self.tokens.view(),
            "cost": self.cost.view(),
        })
    }
}

/// Totals for one group of requests.
#[derive(Debug, Clone, Default)]
pub struct UsageTotals {
    pub key: Option<String>,
    pub requests: i64,
    pub ok: i64,
    pub refused: i64,
    pub errors: i64,
    pub interrupted: i64,
    pub tokens: Tokens,
    /// Answered requests whose provider reported no usage at all.
    pub usage_missing: i64,
    pub latency_ms_total: i64,
    pub priced: i64,
    pub partial: i64,
    pub unpriced: i64,
    pub free: i64,
    pub subscription: i64,
    /// One amount per currency, in micros. Never summed across currencies.
    pub cost: Vec<(String, i64)>,
}

impl UsageTotals {
    pub fn absorb(&mut self, other: UsageTotals) {
        self.requests += other.requests;
        self.ok += other.ok;
        self.refused += other.refused;
        self.errors += other.errors;
        self.interrupted += other.interrupted;
        self.tokens.absorb(other.tokens);
        self.usage_missing += other.usage_missing;
        self.latency_ms_total += other.latency_ms_total;
        self.priced += other.priced;
        self.partial += other.partial;
        self.unpriced += other.unpriced;
        self.free += other.free;
        self.subscription += other.subscription;
        for (currency, micros) in other.cost {
            match self.cost.iter_mut().find(|(c, _)| *c == currency) {
                Some((_, total)) => *total = total.saturating_add(micros),
                None => self.cost.push((currency, micros)),
            }
        }
    }

    pub fn view(&self, key_name: Option<&str>) -> Value {
        let mut view = json!({
            "requests": self.requests,
            "outcomes": {
                "ok": self.ok,
                "refused": self.refused,
                "error": self.errors,
                "interrupted": self.interrupted,
            },
            "tokens": self.tokens.view(),
            "usage_missing": self.usage_missing,
            "latency_ms_avg": if self.requests > 0 { Some(self.latency_ms_total / self.requests) } else { None },
            "cost": self.cost.iter().map(|(currency, micros)| json!({
                "currency": currency,
                "amount": Micros(*micros).exact(),
            })).collect::<Vec<_>>(),
            // Exact only when every answered request on a paid provider was priced in full.
            // Otherwise the amounts are a floor, and saying so is the point.
            "cost_complete": self.partial == 0 && self.unpriced == 0,
            "priced": self.priced,
            "partial": self.partial,
            "unpriced": self.unpriced,
            "free": self.free,
            "subscription": self.subscription,
        });
        if let Some(name) = key_name {
            view[name] = json!(self.key);
        }
        view
    }
}

/// How totals are grouped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Grouping {
    None,
    /// By `provider/model`.
    Model,
    Provider,
    /// By the name the client asked for: a route set or a direct model.
    Asked,
    /// By UTC day.
    Day,
}

impl Grouping {
    pub fn parse(text: &str) -> Option<Grouping> {
        Some(match text {
            "none" => Grouping::None,
            "model" => Grouping::Model,
            "provider" => Grouping::Provider,
            "asked" | "route_set" => Grouping::Asked,
            "day" => Grouping::Day,
            _ => return None,
        })
    }

    /// The SQL expression the rows are grouped by. Fixed strings only, never input.
    pub fn column(self) -> &'static str {
        match self {
            Grouping::None => "NULL",
            Grouping::Model => "route",
            Grouping::Provider => "provider_id",
            Grouping::Asked => "asked",
            Grouping::Day => "date(at, 'unixepoch')",
        }
    }

    /// What the key is called in the reply.
    pub fn key_name(self) -> Option<&'static str> {
        match self {
            Grouping::None => None,
            Grouping::Model => Some("model"),
            Grouping::Provider => Some("provider"),
            Grouping::Asked => Some("asked"),
            Grouping::Day => Some("day"),
        }
    }
}

/// Which rows a query looks at.
#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    /// Unix seconds, inclusive.
    pub from: Option<i64>,
    /// Unix seconds, exclusive.
    pub to: Option<i64>,
    pub provider: Option<String>,
    /// `provider/model`.
    pub model: Option<String>,
    pub asked: Option<String>,
    pub outcome: Option<String>,
}

impl UsageFilter {
    /// The `WHERE` clause, with numbered placeholders matching [`UsageFilter::params`].
    pub fn clause(&self) -> String {
        let mut parts = vec!["1 = 1".to_string()];
        let mut n = 0;
        let mut next = |column: &str, op: &str| {
            n += 1;
            parts.push(format!("{column} {op} ?{n}"));
        };
        if self.from.is_some() {
            next("at", ">=");
        }
        if self.to.is_some() {
            next("at", "<");
        }
        if self.provider.is_some() {
            next("provider_id", "=");
        }
        if self.model.is_some() {
            next("route", "=");
        }
        if self.asked.is_some() {
            next("asked", "=");
        }
        if self.outcome.is_some() {
            next("outcome", "=");
        }
        parts.join(" AND ")
    }

    /// The values for [`UsageFilter::clause`], in the same order.
    pub fn params(&self) -> Vec<&dyn ToSql> {
        let mut params: Vec<&dyn ToSql> = Vec::new();
        if let Some(v) = &self.from {
            params.push(v);
        }
        if let Some(v) = &self.to {
            params.push(v);
        }
        if let Some(v) = &self.provider {
            params.push(v);
        }
        if let Some(v) = &self.model {
            params.push(v);
        }
        if let Some(v) = &self.asked {
            params.push(v);
        }
        if let Some(v) = &self.outcome {
            params.push(v);
        }
        params
    }
}

/// Rows waiting to be written. Enough to ride out a slow disk; past that, rows are dropped.
const QUEUE: usize = 10_000;
/// Rows written per transaction.
const BATCH: usize = 500;

/// Hands usage rows to the writer task.
#[derive(Clone)]
pub struct Recorder {
    tx: mpsc::Sender<UsageRecord>,
}

impl Recorder {
    /// Starts the writer. It runs until every `Recorder` is dropped, then writes what is left
    /// and ends; await the handle to be sure it has.
    pub fn start(db: Db) -> (Recorder, JoinHandle<()>) {
        let (tx, mut rx) = mpsc::channel::<UsageRecord>(QUEUE);
        let writer = tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < BATCH {
                    match rx.try_recv() {
                        Ok(row) => batch.push(row),
                        Err(_) => break,
                    }
                }
                let written = batch.len();
                if let Err(e) = db.run_mut(move |store| store.insert_usage(&batch)).await {
                    tracing::error!(rows = written, error = %e, "usage rows could not be written");
                }
            }
        });
        (Recorder { tx }, writer)
    }

    /// Queues a row. Never waits.
    pub fn record(&self, row: UsageRecord) {
        if let Err(e) = self.tx.try_send(row) {
            tracing::warn!(error = %e, "a usage row was dropped: the writer is behind");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_add_counts_and_keep_currencies_apart() {
        let mut a = UsageTotals {
            requests: 2,
            ok: 2,
            priced: 2,
            tokens: Tokens {
                input: Some(10),
                output: Some(5),
                ..Tokens::default()
            },
            cost: vec![("USD".into(), 1_500)],
            ..UsageTotals::default()
        };
        a.absorb(UsageTotals {
            requests: 1,
            ok: 1,
            free: 1,
            tokens: Tokens {
                input: Some(3),
                cache_read: Some(7),
                ..Tokens::default()
            },
            ..UsageTotals::default()
        });
        a.absorb(UsageTotals {
            requests: 1,
            ok: 1,
            priced: 1,
            cost: vec![("EUR".into(), 200), ("USD".into(), 500)],
            ..UsageTotals::default()
        });
        assert_eq!(a.requests, 4);
        assert_eq!(a.tokens.input, Some(13));
        assert_eq!(a.tokens.cache_read, Some(7));
        assert_eq!(a.cost, vec![("USD".into(), 2_000), ("EUR".into(), 200)]);
        let view = a.view(None);
        assert_eq!(view["cost"][0]["amount"], "0.002000");
        assert_eq!(view["cost_complete"], true);
        assert_eq!(view["tokens"]["total"], 25);
    }

    #[test]
    fn an_unpriced_request_makes_the_total_a_floor() {
        let totals = UsageTotals {
            requests: 2,
            priced: 1,
            unpriced: 1,
            cost: vec![("USD".into(), 100)],
            ..UsageTotals::default()
        };
        assert_eq!(totals.view(None)["cost_complete"], false);
    }

    #[test]
    fn a_cost_round_trips_through_its_row_form() {
        for cost in [
            Cost::Priced {
                micros: 1234,
                currency: "USD".into(),
                partial: false,
            },
            Cost::Priced {
                micros: 1,
                currency: "USD".into(),
                partial: true,
            },
            Cost::Unpriced,
            Cost::Free,
            Cost::None,
        ] {
            let back = Cost::from_row(
                cost.status(),
                cost.micros(),
                cost.currency().map(String::from),
            );
            assert_eq!(back, cost);
        }
        assert_eq!(
            Cost::Priced {
                micros: 1234,
                currency: "USD".into(),
                partial: false
            }
            .header()
            .as_deref(),
            Some("0.001234 USD")
        );
    }

    #[test]
    fn the_filter_clause_and_its_parameters_line_up() {
        let filter = UsageFilter {
            from: Some(1),
            provider: Some("a".into()),
            outcome: Some("ok".into()),
            ..UsageFilter::default()
        };
        assert_eq!(
            filter.clause(),
            "1 = 1 AND at >= ?1 AND provider_id = ?2 AND outcome = ?3"
        );
        assert_eq!(filter.params().len(), 3);
    }
}
