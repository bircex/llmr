//! What a call consumed, and how much of that the provider actually told you.

use serde::{Deserialize, Serialize};

/// How complete a [`Usage`] is.
///
/// Carried alongside the numbers rather than inferred from them, because zero and unknown
/// look the same once they are added up. A total built from partial usage is a total that
/// understates the bill, and nothing downstream can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum UsageCoverage {
    /// Every field was reported.
    Exact,
    /// Some fields were reported and some were not.
    Partial,
    /// Nothing was reported. The call still happened and still cost something.
    Absent,
}

/// The tokens one call consumed.
///
/// Every field is optional because providers differ in what they report, and a missing
/// number is a fact worth keeping. A subscription command line tool usually reports nothing
/// at all, and writing zero in its place turns an unknown cost into a free one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    /// The part of the prompt that was not served from cache.
    ///
    /// Not the size of the prompt. Providers disagree here, and some report a total that
    /// includes the cached read. A provider in this crate subtracts so that this field
    /// always means the same thing, which is what makes two providers comparable.
    pub input_tokens: Option<u64>,
    /// Prompt tokens served from cache, billed at a lower rate.
    pub cache_read_tokens: Option<u64>,
    /// Prompt tokens written to cache, usually billed at a higher rate than input.
    pub cache_write_tokens: Option<u64>,
    /// Tokens produced, including any the model spent thinking.
    ///
    /// Thinking tokens are billed whether or not they are shown, so they belong in the
    /// number you price.
    pub output_tokens: Option<u64>,
}

impl Usage {
    /// A usage with nothing reported.
    ///
    /// This is the right answer when a provider does not measure. It is not the same as
    /// zeros, and [`Usage::coverage`] tells them apart.
    pub fn absent() -> Self {
        Self::default()
    }

    /// Records the part of the prompt that was not served from cache.
    ///
    /// Not the whole prompt. If your provider reports a total that includes the cached
    /// part, subtract before you get here, or two providers will report different numbers
    /// for the same conversation.
    #[must_use]
    pub fn with_input(mut self, tokens: u64) -> Self {
        self.input_tokens = Some(tokens);
        self
    }

    /// Records prompt tokens served from cache.
    #[must_use]
    pub fn with_cache_read(mut self, tokens: u64) -> Self {
        self.cache_read_tokens = Some(tokens);
        self
    }

    /// Records prompt tokens written to cache.
    #[must_use]
    pub fn with_cache_write(mut self, tokens: u64) -> Self {
        self.cache_write_tokens = Some(tokens);
        self
    }

    /// Records tokens produced, thinking included.
    #[must_use]
    pub fn with_output(mut self, tokens: u64) -> Self {
        self.output_tokens = Some(tokens);
        self
    }

    /// How much of this was reported.
    pub fn coverage(&self) -> UsageCoverage {
        let fields = [
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.output_tokens,
        ];
        let present = fields.iter().filter(|f| f.is_some()).count();
        match present {
            0 => UsageCoverage::Absent,
            n if n == fields.len() => UsageCoverage::Exact,
            _ => UsageCoverage::Partial,
        }
    }

    /// Every prompt token, cached or not.
    ///
    /// Returns `None` when no prompt field was reported at all. A missing field among
    /// others counts as zero, because a provider that reports two of three is telling you
    /// the third did not happen.
    pub fn prompt_tokens(&self) -> Option<u64> {
        let parts = [
            self.input_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        ];
        if parts.iter().all(Option::is_none) {
            return None;
        }
        Some(parts.iter().map(|p| p.unwrap_or(0)).sum())
    }

    /// Adds two usages together, field by field.
    ///
    /// A field missing from both stays missing. A field present in either is a number, so
    /// summing a run of calls never turns a reported total into an unknown one.
    pub fn merge(self, other: Usage) -> Usage {
        fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
            match (a, b) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            }
        }
        Usage {
            input_tokens: add(self.input_tokens, other.input_tokens),
            cache_read_tokens: add(self.cache_read_tokens, other.cache_read_tokens),
            cache_write_tokens: add(self.cache_write_tokens, other.cache_write_tokens),
            output_tokens: add(self.output_tokens, other.output_tokens),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Usage {
        Usage {
            input_tokens: Some(100),
            cache_read_tokens: Some(900),
            cache_write_tokens: Some(50),
            output_tokens: Some(20),
        }
    }

    #[test]
    fn nothing_reported_is_absent_rather_than_zero() {
        let nothing = Usage::absent();
        assert_eq!(nothing.coverage(), UsageCoverage::Absent);
        assert_eq!(nothing.prompt_tokens(), None);
        assert_eq!(nothing.output_tokens, None);
    }

    #[test]
    fn some_fields_reported_is_partial() {
        let partial = Usage {
            output_tokens: Some(20),
            ..Usage::absent()
        };
        assert_eq!(partial.coverage(), UsageCoverage::Partial);
    }

    #[test]
    fn every_field_reported_is_exact() {
        assert_eq!(full().coverage(), UsageCoverage::Exact);
    }

    #[test]
    fn the_prompt_total_counts_the_cached_part() {
        assert_eq!(full().prompt_tokens(), Some(1_050));
    }

    #[test]
    fn merging_two_absent_usages_stays_absent() {
        let merged = Usage::absent().merge(Usage::absent());
        assert_eq!(merged.coverage(), UsageCoverage::Absent);
    }

    #[test]
    fn merging_a_reported_call_with_an_unreported_one_keeps_the_number() {
        // A run of ten calls where one provider reports and another does not should not
        // report nothing. What is known stays known.
        let merged = full().merge(Usage::absent());
        assert_eq!(merged.output_tokens, Some(20));
        assert_eq!(merged.coverage(), UsageCoverage::Exact);
    }

    #[test]
    fn merging_adds_field_by_field() {
        let merged = full().merge(full());
        assert_eq!(merged.input_tokens, Some(200));
        assert_eq!(merged.output_tokens, Some(40));
    }
}
