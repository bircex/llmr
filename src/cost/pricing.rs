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

/// What one model costs: per million tokens, and per unit for what is not sold by the token.
///
/// A chat model has only the token rates. An image, speech or transcription model may be
/// sold by the picture, the second of audio or the character instead, and those rates are
/// here beside the token ones rather than in a second table, so one lookup answers for any
/// kind of model. A rate left at zero is one the vendor does not charge by.
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
    /// Each picture produced.
    #[serde(default)]
    pub image: Micros,
    /// Each second of audio, heard or produced.
    #[serde(default)]
    pub audio_second: Micros,
    /// Per million characters of text read aloud.
    #[serde(default)]
    pub character: Micros,
}

impl Rate {
    /// A rate by the token: uncached input, cache reads, cache writes and output, each per
    /// million tokens.
    pub fn tokens(input: Micros, cache_read: Micros, cache_write: Micros, output: Micros) -> Rate {
        Rate {
            input,
            cache_read,
            cache_write,
            output,
            ..Rate::default()
        }
    }

    /// The same rate, also charging this much for each picture.
    #[must_use]
    pub fn with_image(mut self, per_image: Micros) -> Rate {
        self.image = per_image;
        self
    }

    /// The same rate, also charging this much for each second of audio.
    #[must_use]
    pub fn with_audio_second(mut self, per_second: Micros) -> Rate {
        self.audio_second = per_second;
        self
    }

    /// The same rate, also charging this much per million characters.
    #[must_use]
    pub fn with_character(mut self, per_million: Micros) -> Rate {
        self.character = per_million;
        self
    }

    /// Whether any part of it is charged by the token.
    pub fn by_token(&self) -> bool {
        [self.input, self.cache_read, self.cache_write, self.output]
            .iter()
            .any(|m| m.0 != 0)
    }
}

/// What a call produced or consumed that is not counted in tokens.
///
/// Each is `None` when nothing measured it, which is different from zero: a transcription
/// whose length nobody reported is not a transcription of no seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Units {
    /// Pictures returned.
    pub images: Option<u64>,
    /// Audio heard or produced, in milliseconds.
    pub audio_millis: Option<u64>,
    /// Characters of text read aloud.
    pub characters: Option<u64>,
}

impl Units {
    /// Nothing measured.
    pub fn none() -> Units {
        Units::default()
    }

    /// This many pictures.
    #[must_use]
    pub fn with_images(mut self, images: u64) -> Units {
        self.images = Some(images);
        self
    }

    /// This much audio, in milliseconds.
    #[must_use]
    pub fn with_audio_millis(mut self, millis: u64) -> Units {
        self.audio_millis = Some(millis);
        self
    }

    /// This many characters.
    #[must_use]
    pub fn with_characters(mut self, characters: u64) -> Units {
        self.characters = Some(characters);
        self
    }
}

/// A priced table, and where its numbers came from.
///
/// Prices change on the vendor's schedule. A table with no date on it is a table nobody can
/// audit, so every book carries when it took effect and when a person last checked it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
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

    fn zero(m: &Micros) -> bool {
        m.0 == 0
    }

    #[derive(Serialize, Deserialize)]
    struct Row {
        model: String,
        #[serde(default)]
        input: Micros,
        #[serde(default)]
        output: Micros,
        #[serde(default)]
        cache_read: Micros,
        #[serde(default)]
        cache_write: Micros,
        #[serde(default, skip_serializing_if = "zero")]
        image: Micros,
        #[serde(default, skip_serializing_if = "zero")]
        audio_second: Micros,
        #[serde(default, skip_serializing_if = "zero")]
        character: Micros,
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
                image: rate.image,
                audio_second: rate.audio_second,
                character: rate.character,
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
                    image: row.image,
                    audio_second: row.audio_second,
                    character: row.character,
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
    /// An empty book, dated and sourced, for a caller who fills its rows itself.
    ///
    /// Every field that makes a book auditable is an argument, for the reason
    /// [`PriceBook::parse`] refuses a blank one.
    pub fn new(
        id: impl Into<String>,
        provider: impl Into<String>,
        effective_from: impl Into<String>,
        source: impl Into<String>,
        verified_at: impl Into<String>,
        currency: impl Into<String>,
    ) -> PriceBook {
        PriceBook {
            id: id.into(),
            provider: provider.into(),
            effective_from: effective_from.into(),
            source: source.into(),
            verified_at: verified_at.into(),
            expires_on: None,
            currency: currency.into(),
            rates: BTreeMap::new(),
        }
    }

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
        self.price_with(model, usage, &Units::none())
    }

    /// What a call cost, counting what it produced that is not tokens as well.
    ///
    /// Each part of the rate that is not zero is charged by what measured it: the token
    /// rates by `usage`, the picture, audio and character rates by `units`. A part whose
    /// measure is missing is left out and the cost says it is partial, because it is a
    /// floor. When no part of the rate was measured at all the answer is `None`, not zero.
    pub fn price_with(&self, model: &ModelId, usage: &Usage, units: &Units) -> Option<Priced> {
        let rate = self.rate(model)?;

        // Per million tokens, so the product is divided by a million. Integer division
        // truncates, which understates by less than a millionth of a unit per line.
        let per_million = |count: Option<u64>, price: Micros| -> i64 {
            let count = i64::try_from(count.unwrap_or(0)).unwrap_or(i64::MAX);
            count.saturating_mul(price.0) / 1_000_000
        };

        let mut amount: i64 = 0;
        let mut measured = false;
        let mut missing = false;
        let mut coverage = UsageCoverage::Exact;

        if rate.by_token() {
            if usage.coverage() == UsageCoverage::Absent {
                missing = true;
            } else {
                measured = true;
                coverage = usage.coverage();
                amount = per_million(usage.input_tokens, rate.input)
                    .saturating_add(per_million(usage.cache_read_tokens, rate.cache_read))
                    .saturating_add(per_million(usage.cache_write_tokens, rate.cache_write))
                    .saturating_add(per_million(usage.output_tokens, rate.output));
            }
        }

        let mut unit = |price: Micros, count: Option<u64>, per: i64| {
            if price.0 == 0 {
                return;
            }
            match count {
                Some(count) => {
                    measured = true;
                    let count = i64::try_from(count).unwrap_or(i64::MAX);
                    amount = amount.saturating_add(count.saturating_mul(price.0) / per);
                }
                None => missing = true,
            }
        };
        unit(rate.image, units.images, 1);
        unit(rate.audio_second, units.audio_millis, 1_000);
        unit(rate.character, units.characters, 1_000_000);

        if !measured {
            return None;
        }
        if missing {
            coverage = UsageCoverage::Partial;
        }

        Some(Priced {
            amount: Micros(amount),
            currency: self.currency.clone(),
            book: self.id.clone(),
            coverage,
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
            "a table dated in the future is one somebody typed wrong, and saying so beats clamping it to zero"
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
            Rate::tokens(
                Micros(3_000_000),
                Micros(300_000),
                Micros(3_750_000),
                Micros(15_000_000),
            ),
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
        // The case a provider that reports no usage produces on every call. Returning zero
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
            estimated: false,
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

    fn media_book() -> PriceBook {
        let mut book = book();
        // Sold by the picture only, as some image models are.
        book.rates.insert(
            "pictures".into(),
            Rate::default().with_image(Micros(39_000)),
        );
        // Sold by the second, as a transcription model is.
        book.rates.insert(
            "listener".into(),
            Rate::default().with_audio_second(Micros(100)),
        );
        // Sold by the character, as a text to speech model is.
        book.rates.insert(
            "reader".into(),
            Rate::default().with_character(Micros(15_000_000)),
        );
        book
    }

    #[test]
    fn a_picture_a_second_and_a_character_are_each_priced_by_what_measured_them() {
        let book = media_book();
        let none = Usage::absent();

        let two = book.price_with(&"pictures".into(), &none, &Units::none().with_images(2));
        assert_eq!(two.as_ref().map(|p| p.amount), Some(Micros(78_000)));
        assert_eq!(two.map(|p| p.coverage), Some(UsageCoverage::Exact));

        // A minute and a half at a hundredth of a cent a second.
        let heard = book.price_with(
            &"listener".into(),
            &none,
            &Units::none().with_audio_millis(90_500),
        );
        assert_eq!(heard.map(|p| p.amount), Some(Micros(9_050)));

        // Two thousand characters at fifteen dollars a million.
        let read = book.price_with(
            &"reader".into(),
            &none,
            &Units::none().with_characters(2_000),
        );
        assert_eq!(read.map(|p| p.amount), Some(Micros(30_000)));
    }

    #[test]
    fn a_model_sold_by_a_unit_nobody_measured_is_unpriced_not_free() {
        // The transcription whose length was not reported. Zero would be a guess; `None`
        // is what is known.
        assert_eq!(
            media_book().price_with(&"listener".into(), &Usage::absent(), &Units::none()),
            None
        );
    }

    #[test]
    fn a_rate_with_tokens_and_pictures_is_partial_when_only_one_was_measured() {
        let mut book = book();
        book.rates.insert(
            "both".into(),
            Rate::tokens(Micros(5_000_000), Micros(0), Micros(0), Micros(0))
                .with_image(Micros(10_000)),
        );
        let usage = Usage {
            input_tokens: Some(1_000_000),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(0),
            estimated: false,
        };
        let priced = book.price_with(&"both".into(), &usage, &Units::none());
        assert_eq!(priced.as_ref().map(|p| p.amount), Some(Micros(5_000_000)));
        assert_eq!(
            priced.map(|p| p.coverage),
            Some(UsageCoverage::Partial),
            "the pictures were not counted, so the amount is a floor"
        );
    }

    #[test]
    fn a_book_row_reads_unit_prices_and_writes_back_only_those_it_has() {
        let book = PriceBook::parse(
            "id = \"x\"\nprovider = \"p\"\neffective_from = \"2026-01-01\"\n\
             source = \"s\"\nverified_at = \"2026-01-01\"\ncurrency = \"USD\"\n\
             [[price]]\nmodel = \"whisper\"\naudio_second = \"0.0001\"\n",
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            book.rate(&"whisper".into()).map(|r| r.audio_second),
            Some(Micros(100))
        );
        let written = toml::to_string(&book).unwrap_or_default();
        assert!(written.contains("audio_second"));
        assert!(!written.contains("image"));
    }

    impl Priced {
        fn exact_for_test(&self) -> String {
            self.amount.exact()
        }
    }
}
