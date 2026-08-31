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
    /// The numbers were counted here rather than reported by the provider.
    ///
    /// Between [`UsageCoverage::Exact`] and [`UsageCoverage::Partial`] in this ordering,
    /// and it is a different kind of wrong from either. A partial usage understates: the
    /// bill is that much or more. An estimate can be wrong in **either** direction, so a
    /// total containing one is not a floor and must not be reported as one.
    ///
    /// This variant exists so that a locally counted number can be added up without being
    /// folded into `Exact`, which would destroy the one property this type is for.
    Estimated,
    /// Some fields were reported and some were not.
    Partial,
    /// Nothing was reported. The call still happened and still cost something.
    Absent,
}

impl UsageCoverage {
    /// How a coverage is written down, in records and in spans.
    ///
    /// One spelling in one place, for the reason [`crate::Reach::as_str`] has one: two
    /// copies of this mapping is two chances for a log line and a report to disagree about
    /// what "absent" means.
    pub fn as_str(self) -> &'static str {
        match self {
            UsageCoverage::Exact => "exact",
            UsageCoverage::Estimated => "estimated",
            UsageCoverage::Partial => "partial",
            UsageCoverage::Absent => "absent",
        }
    }
}

impl std::fmt::Display for UsageCoverage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// Whether these numbers were counted here rather than reported.
    ///
    /// A flag rather than a fifth optional field, because it is a fact about all four at
    /// once. Set it with [`Usage::estimating`], read it through [`Usage::coverage`], and see
    /// [`UsageCoverage::Estimated`] for why an estimate is not a floor.
    pub estimated: bool,
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

    /// Marks these numbers as counted here rather than reported by the provider.
    ///
    /// The case is a subscription command line tool: it answers, it was billed, and it says
    /// nothing about tokens. A caller with a token counter can produce a number, and this is
    /// how that number is kept apart from one a vendor reported.
    ///
    /// ```
    /// # use llmr::{Usage, UsageCoverage};
    /// let counted = Usage::absent()
    ///     .with_input(1_200)
    ///     .with_cache_read(0)
    ///     .with_cache_write(0)
    ///     .with_output(340)
    ///     .estimating();
    ///
    /// assert_eq!(counted.coverage(), UsageCoverage::Estimated);
    /// ```
    ///
    /// **This crate does not count tokens for you and will not.** A tokeniser has to match
    /// the vendor's, per model, and one that is close produces numbers that look right and
    /// are not, which is the failure every type in this module exists to prevent. Bring your
    /// own count, or leave the call [`Usage::absent`].
    ///
    /// Estimating nothing is still nothing: this on an otherwise absent usage leaves the
    /// coverage [`UsageCoverage::Absent`], because there is no estimate to be approximate
    /// about.
    #[must_use]
    pub fn estimating(mut self) -> Self {
        self.estimated = true;
        self
    }

    /// What an embedding call consumed, when the provider reported its prompt tokens.
    ///
    /// An embedding endpoint reports one number, because one number is all there is: text
    /// goes in and a vector comes out, and a vector is not tokens. So the other three fields
    /// are set to zero rather than left absent, and this reads as
    /// [`UsageCoverage::Exact`].
    ///
    /// **That is a claim, and it is the same claim [`Usage::prompt_tokens`] already makes**:
    /// a provider reporting some fields and not others is saying the others did not happen.
    /// Here they did not. Left absent instead, every embedding call would read as `Partial`
    /// and turn every [`crate::Ledger`] total into a floor forever — an honest-looking
    /// hedge about a call that was measured exactly.
    ///
    /// If a vendor does report cached tokens on an embedding call, build the [`Usage`] with
    /// the builders instead. This constructor is for the common shape, not for every shape.
    pub fn embedding(input_tokens: u64) -> Self {
        Usage {
            input_tokens: Some(input_tokens),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
            output_tokens: Some(0),
            estimated: false,
        }
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
            // Nothing to be approximate about, so the flag changes nothing here.
            0 => UsageCoverage::Absent,
            // An estimate of three fields out of four is still an estimate, and `Estimated`
            // already says the number can be wrong in either direction, which covers the
            // field that was never counted. Reporting `Partial` instead would claim the
            // total is a floor, and an estimate is not one.
            _ if self.estimated => UsageCoverage::Estimated,
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
            // A measured call added to an estimated one is a number that is partly guessed,
            // and the sum has to say so. Estimation spreads on merge, exactly as absence
            // does not.
            estimated: self.estimated || other.estimated,
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
            estimated: false,
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

    #[test]
    fn a_locally_counted_call_is_estimated_rather_than_exact() {
        // Folding a count into `Exact` would destroy the one property this type is for. It
        // is a different kind of wrong from `Partial` too: partial understates, an estimate
        // can be wrong either way.
        let counted = full().estimating();
        assert_eq!(counted.coverage(), UsageCoverage::Estimated);
        assert_eq!(counted.prompt_tokens(), Some(1_050), "still a number");
    }

    #[test]
    fn an_estimate_of_some_fields_is_still_an_estimate_rather_than_partial() {
        // `Partial` claims the figure is a floor, and an estimate is not one whether or not
        // every field was counted.
        let some = Usage::absent().with_output(20).estimating();
        assert_eq!(some.coverage(), UsageCoverage::Estimated);
    }

    #[test]
    fn estimating_nothing_is_still_nothing() {
        // There is no estimate to be approximate about, so the flag changes no answer here.
        assert_eq!(
            Usage::absent().estimating().coverage(),
            UsageCoverage::Absent
        );
    }

    #[test]
    fn a_measured_call_merged_with_an_estimated_one_is_estimated() {
        // The sum is partly guessed and has to say so, or the guess disappears into the
        // measurement the first time two calls are added up.
        assert_eq!(
            full().merge(full().estimating()).coverage(),
            UsageCoverage::Estimated
        );
        assert_eq!(full().merge(full()).coverage(), UsageCoverage::Exact);
    }

    #[test]
    fn the_coverages_are_ordered_best_first() {
        // Read by anything comparing two calls. Exact beats an estimate, an estimate beats
        // a figure that is short, and all three beat nothing.
        assert!(UsageCoverage::Exact < UsageCoverage::Estimated);
        assert!(UsageCoverage::Estimated < UsageCoverage::Partial);
        assert!(UsageCoverage::Partial < UsageCoverage::Absent);
    }

    #[test]
    fn an_embedding_call_is_measured_exactly_rather_than_partly() {
        // One number is all an embedding endpoint has to report, and a vector is not
        // tokens. Left absent, the three fields that did not happen would make every
        // embedding call `Partial` and every ledger total a floor for good.
        let usage = Usage::embedding(1_500);
        assert_eq!(usage.coverage(), UsageCoverage::Exact);
        assert_eq!(usage.prompt_tokens(), Some(1_500));
        assert_eq!(usage.output_tokens, Some(0));
    }

    #[test]
    fn an_embedding_nobody_measured_is_still_absent() {
        // The constructor is for a reported number. A provider that got none uses
        // `absent`, and zero is not a substitute for it here any more than anywhere else.
        assert_eq!(Usage::absent().coverage(), UsageCoverage::Absent);
        assert_eq!(Usage::embedding(0).coverage(), UsageCoverage::Exact);
    }
}
