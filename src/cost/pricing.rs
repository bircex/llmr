//! What a call cost, as dated data rather than a constant.

use crate::cost::usage::{Usage, UsageCoverage};
use crate::model::ModelId;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// An amount of money, in millionths of the currency unit.
///
/// Integers all the way down. Token prices have six significant decimal places and a binary
/// float cannot hold `0.1` exactly, so a total built by adding floats drifts. Millionths of
/// a dollar hold every published price without rounding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Micros(pub i64);

impl Micros {
    /// Reads an amount written the way a price list writes it, such as `15.00` or `0.30`.
    ///
    /// # Errors
    ///
    /// Returns a message when the text is not a decimal number. More than six decimal
    /// places is an error rather than a rounding, because a price with seven places is one
    /// somebody copied from a different unit and the rounded version would look right.
    pub fn parse(text: &str) -> std::result::Result<Micros, String> {
        let text = text.trim();
        let (negative, digits) = match text.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, text),
        };

        let (whole, fraction) = match digits.split_once('.') {
            Some((w, f)) => (w, f),
            None => (digits, ""),
        };
        if whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("{text:?} is not a decimal number"));
        }
        if !fraction.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("{text:?} is not a decimal number"));
        }
        if fraction.len() > 6 {
            return Err(format!(
                "{text:?} has more than six decimal places. A price written to seven is one \
                 copied from a different unit, and rounding it would look correct"
            ));
        }

        let whole: i64 = whole
            .parse()
            .map_err(|_| format!("{text:?} is too large to hold"))?;
        let mut padded = fraction.to_string();
        while padded.len() < 6 {
            padded.push('0');
        }
        let fraction: i64 = if padded.is_empty() {
            0
        } else {
            padded
                .parse()
                .map_err(|_| format!("{text:?} is not a decimal number"))?
        };

        let total = whole
            .checked_mul(1_000_000)
            .and_then(|w| w.checked_add(fraction))
            .ok_or_else(|| format!("{text:?} is too large to hold"))?;
        Ok(Micros(if negative { -total } else { total }))
    }

    /// The amount, written out with six decimal places.
    ///
    /// Six rather than two. Rounding a per call cost to cents turns most calls into zero,
    /// and a column of zeros adds up to nothing.
    pub fn exact(self) -> String {
        let sign = if self.0 < 0 { "-" } else { "" };
        let n = self.0.unsigned_abs();
        format!("{sign}{}.{:06}", n / 1_000_000, n % 1_000_000)
    }
}

impl Serialize for Micros {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.exact())
    }
}

impl<'de> Deserialize<'de> for Micros {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let text = String::deserialize(deserializer)?;
        Micros::parse(&text).map_err(D::Error::custom)
    }
}

impl std::fmt::Display for Micros {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.exact())
    }
}

impl std::ops::Add for Micros {
    type Output = Micros;
    fn add(self, other: Micros) -> Micros {
        Micros(self.0.saturating_add(other.0))
    }
}

/// What one model costs, per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Rate {
    /// Uncached prompt tokens.
    pub input: Micros,
    /// Prompt tokens served from cache.
    pub cache_read: Micros,
    /// Prompt tokens written to cache.
    pub cache_write: Micros,
    /// Tokens produced.
    pub output: Micros,
}

/// A priced table, and where its numbers came from.
///
/// Prices change on the vendor's schedule. A table with no date on it is a table nobody can
/// audit, so every book carries when it took effect and when a person last checked it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PriceBook {
    /// A name for this edition, recorded beside anything it priced.
    pub id: String,
    /// Which vendor these prices are for.
    pub provider: String,
    /// The date these prices took effect, as `YYYY-MM-DD`.
    pub effective_from: String,
    /// Where the numbers came from. A published page, an invoice, a contract.
    pub source: String,
    /// When a person last checked them, as `YYYY-MM-DD`.
    pub verified_at: String,
    /// The date after which these numbers are known to be wrong, as `YYYY-MM-DD`.
    ///
    /// Not for a book that might have drifted; that is what [`PriceBook::age`] is for. This
    /// is for a book that has been told when it stops being right: an introductory rate the
    /// vendor has already published an end date for, a contract that runs out, a quarter's
    /// negotiated pricing. `None` when nothing said.
    ///
    /// It belongs to the book rather than the row because one row going wrong is enough to
    /// make the edition wrong, and a caller who has to check per model will not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_on: Option<String>,
    /// The currency, as an ISO code such as `USD`.
    pub currency: String,
    /// Rates by model name.
    #[serde(default, rename = "price", with = "rows")]
    pub rates: BTreeMap<String, Rate>,
}

/// A price file writes an array of tables; this reads it into a map keyed by model.
///
/// A model listed twice is refused. Whichever row came last would win, and nothing anywhere
/// would say that the other one had been overruled.
mod rows {
    use super::{Micros, Rate};
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    #[derive(Serialize, Deserialize)]
    struct Row {
        model: String,
        input: Micros,
        output: Micros,
        #[serde(default)]
        cache_read: Micros,
        #[serde(default)]
        cache_write: Micros,
    }

    pub fn serialize<S: Serializer>(
        rates: &BTreeMap<String, Rate>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        rates
            .iter()
            .map(|(model, rate)| Row {
                model: model.clone(),
                input: rate.input,
                output: rate.output,
                cache_read: rate.cache_read,
                cache_write: rate.cache_write,
            })
            .collect::<Vec<_>>()
            .serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, Rate>, D::Error> {
        let rows = Vec::<Row>::deserialize(deserializer)?;
        let mut rates = BTreeMap::new();
        for row in rows {
            if rates.contains_key(&row.model) {
                return Err(D::Error::custom(format!(
                    "{} is priced twice. One row would silently overrule the other",
                    row.model
                )));
            }
            rates.insert(
                row.model,
                Rate {
                    input: row.input,
                    output: row.output,
                    cache_read: row.cache_read,
                    cache_write: row.cache_write,
                },
            );
        }
        Ok(rates)
    }
}

/// A cost, and how much of it rests on numbers that were actually reported.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Priced {
    /// The amount.
    pub amount: Micros,
    /// What the amount is denominated in, copied from the book that priced it.
    ///
    /// An ISO code such as `USD`. [`Micros`] is a bare integer and two of them add whatever
    /// they are: without this field a ledger holding one call priced in dollars and one in
    /// euros would produce a number that is neither.
    pub currency: String,
    /// Which price book edition produced it.
    pub book: String,
    /// How complete the usage behind it was.
    ///
    /// A cost from partial usage understates the bill. Carrying the coverage means a total
    /// can say so instead of looking exact.
    pub coverage: UsageCoverage,
}

/// Why a price book should be checked again before anything is billed against it.
///
/// A reason rather than a boolean, for the same reason [`crate::router::Attempted`] carries
/// one: "this table is stale" and "this table expired eleven days ago" call for different
/// things, and a program that cannot tell them apart cannot act on either.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Recheck {
    /// Past the date the book itself said its numbers stop being right.
    ///
    /// The one that is not a judgement call. Somebody published an end date and it has
    /// passed, so every figure this book produces from here is wrong by whatever changed.
    Expired {
        /// The date the book named, as `YYYY-MM-DD`.
        on: String,
        /// How long ago that was, in days.
        days_ago: i64,
    },
    /// Nobody has checked these numbers in longer than the rule allows.
    Aged {
        /// Days since [`PriceBook::verified_at`].
        days: i64,
    },
    /// A date on this book cannot be read, so its age is unknowable.
    ///
    /// Reported rather than ignored. A book whose date is `"soon"` is a book that would
    /// otherwise never age, and never ageing is exactly the failure the dates exist to stop.
    Undatable {
        /// Which field could not be read.
        field: &'static str,
    },
}

impl std::fmt::Display for Recheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Recheck::Expired { on, days_ago } => {
                write!(f, "expired on {on}, {days_ago} days ago")
            }
            Recheck::Aged { days } => write!(f, "last checked {days} days ago"),
            Recheck::Undatable { field } => write!(f, "{field} is not a date this can read"),
        }
    }
}

/// A `YYYY-MM-DD` date as a day number, so two of them can be subtracted.
///
/// Days since 1970-01-01, by the civil calendar algorithm, which is exact for every date
/// this crate will ever be handed and needs no clock and no dependency. `None` for anything
/// that is not three numbers in that shape.
fn day_number(text: &str) -> Option<i64> {
    let mut parts = text.trim().splitn(3, '-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    // March is treated as the first month, which puts the leap day at the end of the year
    // and removes every special case from the arithmetic below.
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted = (month + 9) % 12;
    let day_of_year = (153 * shifted + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

impl PriceBook {
    /// How long this crate lets a price table go unchecked before it says so: 90 days.
    ///
    /// A rule rather than a guess dressed as one. Vendors change prices on their own
    /// schedule and none of them tell this crate, so any number here is arbitrary. What
    /// makes it useful is that it is written down, it is one number, and
    /// [`PriceBook::needs_rechecking`] applies it for you.
    pub const RECHECK_AFTER_DAYS: i64 = 90;

    /// How many days since a person last checked these numbers, as of `today`.
    ///
    /// `today` is an argument because this crate does not read a clock. Every date in a
    /// table is already `YYYY-MM-DD` text, so the comparison is between two things of the
    /// same kind, and a test can ask what this book looks like in 2027 without waiting.
    ///
    /// `None` when [`PriceBook::verified_at`] is not a date this can read. Negative when
    /// the book is dated in the future, which is a table somebody typed wrong.
    pub fn age(&self, today: &str) -> Option<i64> {
        Some(day_number(today)? - day_number(&self.verified_at)?)
    }

    /// Whether this book should be checked again, and why.
    ///
    /// `None` means it is inside its own expiry and inside [`PriceBook::RECHECK_AFTER_DAYS`].
    /// Anything else is a [`Recheck`] naming the reason.
    ///
    /// # What this is for
    ///
    /// Silent staleness. A price that is quietly six months old produces a confident bill
    /// that is wrong by whatever the vendor changed, and nothing downstream can tell,
    /// because the arithmetic is correct and the number has the right number of decimal
    /// places. Call this once at startup beside [`crate::Router::unusable`], and again
    /// wherever a total is presented as a bill.
    ///
    /// ```
    /// # use llmr::cost::pricing::{PriceBook, Recheck};
    /// # let book = PriceBook::parse(r#"
    /// # id = "b"
    /// # provider = "p"
    /// # effective_from = "2026-01-01"
    /// # source = "a page"
    /// # verified_at = "2026-01-01"
    /// # currency = "USD"
    /// # "#).unwrap();
    /// assert_eq!(book.needs_rechecking("2026-02-01"), None);
    /// assert!(matches!(
    ///     book.needs_rechecking("2026-09-01"),
    ///     Some(Recheck::Aged { .. })
    /// ));
    /// ```
    pub fn needs_rechecking(&self, today: &str) -> Option<Recheck> {
        let Some(now) = day_number(today) else {
            return Some(Recheck::Undatable { field: "today" });
        };

        // Expiry first. A book that said when it stops being right has settled the question,
        // and reporting its age instead would be reporting the weaker of two facts.
        if let Some(expires) = &self.expires_on {
            let Some(end) = day_number(expires) else {
                return Some(Recheck::Undatable {
                    field: "expires_on",
                });
            };
            if now > end {
                return Some(Recheck::Expired {
                    on: expires.clone(),
                    days_ago: now - end,
                });
            }
        }

        let Some(checked) = day_number(&self.verified_at) else {
            return Some(Recheck::Undatable {
                field: "verified_at",
            });
        };
        let days = now - checked;
        (days >= Self::RECHECK_AFTER_DAYS).then_some(Recheck::Aged { days })
    }

    /// Reads a price book from TOML.
    ///
    /// # Errors
    ///
    /// Returns a message when the document cannot be parsed, or when a field that makes the
    /// book auditable is blank. A book with no source and no date is a set of numbers
    /// somebody typed, and there is no way to check it later.
    pub fn parse(text: &str) -> std::result::Result<PriceBook, String> {
        let book: PriceBook = toml::from_str(text).map_err(|e| e.to_string())?;
        for (field, value) in [
            ("id", &book.id),
            ("provider", &book.provider),
            ("effective_from", &book.effective_from),
            ("source", &book.source),
            ("verified_at", &book.verified_at),
            ("currency", &book.currency),
        ] {
            if value.trim().is_empty() {
                return Err(format!(
                    "{field} is blank. A price nobody can date or trace is a number nobody \
                     can check"
                ));
            }
        }
        Ok(book)
    }

    /// The rate for a model, if this book has one.
    pub fn rate(&self, model: &ModelId) -> Option<&Rate> {
        self.rates.get(model.as_str())
    }

    /// What a call cost.
    ///
    /// Returns `None` when this book has no rate for the model, or when the provider
    /// reported no usage at all. Both are honest answers, and both are better than a zero
    /// that adds into a total as though the call were free.
    pub fn price(&self, model: &ModelId, usage: &Usage) -> Option<Priced> {
        let rate = self.rate(model)?;
        if usage.coverage() == UsageCoverage::Absent {
            return None;
        }

        // Per million tokens, so the product is divided by a million. Integer division
        // truncates, which understates by less than a millionth of a unit per line.
        let part = |tokens: Option<u64>, per_million: Micros| -> i64 {
            let tokens = i64::try_from(tokens.unwrap_or(0)).unwrap_or(i64::MAX);
            tokens.saturating_mul(per_million.0) / 1_000_000
        };

        let amount = part(usage.input_tokens, rate.input)
            .saturating_add(part(usage.cache_read_tokens, rate.cache_read))
            .saturating_add(part(usage.cache_write_tokens, rate.cache_write))
            .saturating_add(part(usage.output_tokens, rate.output));

        Some(Priced {
            amount: Micros(amount),
            currency: self.currency.clone(),
            book: self.id.clone(),
            coverage: usage.coverage(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A book with the dates a test wants and no rows.
    fn dated(verified_at: &str, expires_on: Option<&str>) -> PriceBook {
        PriceBook {
            id: "b".into(),
            provider: "p".into(),
            effective_from: "2026-01-01".into(),
            source: "a published page".into(),
            verified_at: verified_at.into(),
            expires_on: expires_on.map(Into::into),
            currency: "USD".into(),
            rates: BTreeMap::new(),
        }
    }

    #[test]
    fn a_day_number_counts_from_the_epoch_and_gets_the_leap_years_right() {
        // Fixed points anybody can check, and the two the arithmetic gets wrong when the
        // year is not shifted to start in March.
        assert_eq!(day_number("1970-01-01"), Some(0));
        assert_eq!(day_number("1970-01-02"), Some(1));
        assert_eq!(
            day_number("2000-03-01"),
            day_number("2000-02-29").map(|d| d + 1)
        );
        assert_eq!(
            day_number("2100-03-01"),
            day_number("2100-02-28").map(|d| d + 1)
        );
        assert_eq!(day_number("1969-12-31"), Some(-1));
    }

    #[test]
    fn a_date_that_is_not_one_is_refused_rather_than_read_as_zero() {
        // The failure that matters: a date read as day zero makes every book fifty years
        // stale, and a date read as "today" makes every book eternally fresh. Neither is a
        // number, so neither is returned.
        assert_eq!(day_number("soon"), None);
        assert_eq!(day_number("2026-13-01"), None);
        assert_eq!(day_number("2026-00-10"), None);
        assert_eq!(day_number("2026-08"), None);
        assert_eq!(day_number(""), None);
    }

    #[test]
    fn a_books_age_is_days_since_a_person_last_checked_it() {
        let book = dated("2026-08-01", None);
        assert_eq!(book.age("2026-08-31"), Some(30));
        assert_eq!(book.age("2026-08-01"), Some(0));
        assert_eq!(
            book.age("2026-07-31"),
            Some(-1),
            "a table dated in the future is one somebody typed wrong, and saying so beats              clamping it to zero"
        );
    }

    #[test]
    fn a_book_nobody_has_checked_in_a_quarter_asks_to_be_checked() {
        let book = dated("2026-08-01", None);
        assert_eq!(book.needs_rechecking("2026-09-01"), None);
        assert_eq!(
            book.needs_rechecking("2026-10-30"),
            Some(Recheck::Aged { days: 90 }),
            "the rule is >= RECHECK_AFTER_DAYS, and the boundary is part of the rule"
        );
    }

    #[test]
    fn an_expiry_the_book_announced_wins_over_its_age() {
        // Both are true past the end date. The one worth reporting is the settled one:
        // ageing says somebody should look, expiry says the numbers have already changed.
        let book = dated("2026-08-01", Some("2026-12-31"));
        assert_eq!(
            book.needs_rechecking("2027-01-02"),
            Some(Recheck::Expired {
                on: "2026-12-31".into(),
                days_ago: 2
            })
        );
        assert_eq!(
            book.needs_rechecking("2026-12-31"),
            Some(Recheck::Aged { days: 152 }),
            "on the last good day it has not expired, though it has certainly aged"
        );
    }

    #[test]
    fn a_book_whose_date_cannot_be_read_reports_that_rather_than_ageing_forever() {
        // The quiet failure. A `verified_at` of "recently" parses as TOML and would make
        // this book permanently fresh, which is the one answer that can never be checked.
        assert_eq!(
            dated("recently", None).needs_rechecking("2026-08-31"),
            Some(Recheck::Undatable {
                field: "verified_at"
            })
        );
        assert_eq!(
            dated("2026-08-01", Some("when the contract ends")).needs_rechecking("2026-08-02"),
            Some(Recheck::Undatable {
                field: "expires_on"
            })
        );
        assert_eq!(
            dated("2026-08-01", None).needs_rechecking("today"),
            Some(Recheck::Undatable { field: "today" })
        );
    }

    fn book() -> PriceBook {
        let mut rates = BTreeMap::new();
        rates.insert(
            "test-model".to_string(),
            Rate {
                input: Micros(3_000_000),
                cache_read: Micros(300_000),
                cache_write: Micros(3_750_000),
                output: Micros(15_000_000),
            },
        );
        PriceBook {
            id: "test-2026-08".into(),
            provider: "test".into(),
            effective_from: "2026-08-01".into(),
            source: "docs".into(),
            verified_at: "2026-08-28".into(),
            expires_on: None,
            currency: "USD".into(),
            rates,
        }
    }

    #[test]
    fn a_call_the_provider_did_not_measure_has_no_price() {
        // The case a subscription command line tool produces on every call. Returning zero
        // would make an unknown cost look like a free one.
        assert_eq!(book().price(&"test-model".into(), &Usage::absent()), None);
    }

    #[test]
    fn a_model_this_book_does_not_list_has_no_price() {
        let usage = Usage {
            output_tokens: Some(1_000),
            ..Usage::absent()
        };
        assert_eq!(book().price(&"some-other-model".into(), &usage), None);
    }

    #[test]
    fn a_priced_call_names_the_book_that_priced_it() {
        let usage = Usage {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(1_000_000),
        };
        let priced = book()
            .price(&"test-model".into(), &usage)
            .unwrap_or(Priced {
                amount: Micros(0),
                currency: "none".into(),
                book: "none".into(),
                coverage: UsageCoverage::Absent,
            });
        assert_eq!(priced.amount, Micros(18_000_000));
        assert_eq!(priced.exact_for_test(), "18.000000");
        assert_eq!(priced.book, "test-2026-08");
        assert_eq!(priced.coverage, UsageCoverage::Exact);
    }

    #[test]
    fn a_priced_call_says_what_the_amount_is_denominated_in() {
        // Without it, adding two costs from two books produces a number in no currency at
        // all. The book already knows; this is that fact travelling with the amount.
        let usage = Usage {
            output_tokens: Some(1_000_000),
            ..Usage::absent()
        };
        assert_eq!(
            book()
                .price(&"test-model".into(), &usage)
                .map(|p| p.currency),
            Some("USD".to_string())
        );
    }

    #[test]
    fn a_cost_from_partial_usage_says_it_is_partial() {
        let usage = Usage {
            output_tokens: Some(1_000_000),
            ..Usage::absent()
        };
        let priced = book().price(&"test-model".into(), &usage);
        assert_eq!(
            priced.map(|p| p.coverage),
            Some(UsageCoverage::Partial),
            "a total built from partial usage understates the bill and has to say so"
        );
    }

    #[test]
    fn a_book_with_no_source_is_refused() {
        let refused = PriceBook::parse(
            "id = \"x\"\nprovider = \"p\"\neffective_from = \"2026-01-01\"\n\
             source = \"\"\nverified_at = \"2026-01-01\"\ncurrency = \"USD\"\n",
        );
        assert!(refused.is_err());
    }

    #[test]
    fn money_is_written_to_six_places_rather_than_two() {
        // Rounding a per call cost to cents turns most calls into zero.
        assert_eq!(Micros(1_234).exact(), "0.001234");
        assert_eq!(Micros(-2_500_000).exact(), "-2.500000");
    }

    impl Priced {
        fn exact_for_test(&self) -> String {
            self.amount.exact()
        }
    }
}
