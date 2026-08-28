//! What a call cost, as dated data rather than a constant.

use crate::model::ModelId;
use crate::usage::{Usage, UsageCoverage};
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
pub struct Priced {
    /// The amount.
    pub amount: Micros,
    /// Which price book edition produced it.
    pub book: String,
    /// How complete the usage behind it was.
    ///
    /// A cost from partial usage understates the bill. Carrying the coverage means a total
    /// can say so instead of looking exact.
    pub coverage: UsageCoverage,
}

impl PriceBook {
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
            book: self.id.clone(),
            coverage: usage.coverage(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                book: "none".into(),
                coverage: UsageCoverage::Absent,
            });
        assert_eq!(priced.amount, Micros(18_000_000));
        assert_eq!(priced.exact_for_test(), "18.000000");
        assert_eq!(priced.book, "test-2026-08");
        assert_eq!(priced.coverage, UsageCoverage::Exact);
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
