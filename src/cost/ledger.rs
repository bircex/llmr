//! What a run cost, and what it cannot say it cost.
//!
//! [`crate::Usage::merge`] adds two calls together. This adds up a run, and the arithmetic
//! is the easy half. The hard half is that a run almost always contains a call nobody
//! measured, and a number that quietly leaves it out is worse than no number at all.
//!
//! # Three rules, and none of them is about adding
//!
//! **A total containing an unpriced call is a lower bound.** [`Total`] says which it is, so
//! a report can print "at least" rather than a figure somebody will read as the bill.
//!
//! **A call nobody could price still happened.** A subscription command line tool reports no
//! usage and has no price row on purpose. It belongs in the count as a call whose cost is
//! unknown — not as zero, and not missing from the ledger, because "we made forty calls and
//! know what thirty of them cost" is a different sentence from "we made thirty calls".
//!
//! **A cost is priced once.** [`Ledger::record`] prices at the moment of recording and keeps
//! the [`Priced`], which carries the edition that produced it. Re-pricing later against a
//! newer table would destroy the record the ledger exists to keep: what it cost *then*.
//!
//! ```
//! use llmr::cost::ledger::Ledger;
//! # use llmr::{ChatResponse, Message, Role, StopReason, Usage};
//! # fn reply() -> ChatResponse {
//! #     ChatResponse::new(
//! #         Message { role: Role::Assistant, content: vec![] },
//! #         StopReason::EndTurn, Usage::absent(), "m".into())
//! # }
//! let mut ledger = Ledger::new();
//! ledger.record(&reply(), None);
//!
//! assert_eq!(ledger.calls(), 1);
//! assert_eq!(ledger.unpriced(), 1);
//! assert!(!ledger.total().is_exact(), "one unpriced call makes it a floor");
//! ```

use crate::chat::response::ChatResponse;
use crate::cost::pricing::{Micros, PriceBook, Priced};
use crate::cost::usage::{Usage, UsageCoverage};
use crate::model::ModelId;

/// What a run cost, and whether that is the whole of it.
///
/// Two variants rather than a number and a flag, because a flag is something a caller can
/// forget to read and a variant is something they have to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Total {
    /// Every call was priced, from usage the provider reported in full.
    Exact(Micros),
    /// At least this much.
    ///
    /// Something in the run was not priced, or was priced from usage the provider only
    /// partly reported. The real figure is this or more, never less.
    AtLeast(Micros),
}

impl Total {
    /// The number, whichever kind it is.
    ///
    /// Read [`Total::is_exact`] before you present it as a bill.
    pub fn amount(self) -> Micros {
        match self {
            Total::Exact(amount) | Total::AtLeast(amount) => amount,
        }
    }

    /// Whether this is the whole cost rather than a floor under it.
    pub fn is_exact(self) -> bool {
        matches!(self, Total::Exact(_))
    }
}

impl std::fmt::Display for Total {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Total::Exact(amount) => write!(f, "{amount}"),
            Total::AtLeast(amount) => write!(f, "at least {amount}"),
        }
    }
}

/// One call, as the ledger remembers it.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Line {
    /// Which model served it, as the reply said.
    pub model: ModelId,
    /// What it consumed, as far as the provider reported.
    pub usage: Usage,
    /// What it cost, when it could be priced.
    ///
    /// `None` means the call happened and its cost is not known: no price row, or usage the
    /// provider never reported. It is deliberately not `Some(zero)`.
    pub cost: Option<Priced>,
}

/// Every call in a run, and what they came to.
///
/// Not thread safe on its own, deliberately: a ledger is a record somebody keeps, and
/// wrapping it in a lock here would decide for every caller how a run is stitched together.
/// Share one behind whatever your program already uses, or keep one per task and
/// [`Ledger::absorb`] them at the end.
#[derive(Debug, Clone, Default)]
pub struct Ledger {
    lines: Vec<Line>,
}

impl Ledger {
    /// An empty ledger.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a reply, pricing it now against this book.
    ///
    /// Pass `None` for a reach that has no price list — a subscription command line tool is
    /// the case this crate ships. The call is still counted; only its cost is unknown.
    ///
    /// Pricing happens here and never again. What is kept is the [`Priced`], carrying the
    /// edition that produced it, so a later change to a table cannot rewrite what a call
    /// cost at the time it was made.
    pub fn record(&mut self, reply: &ChatResponse, book: Option<&PriceBook>) {
        let cost = book.and_then(|book| book.price(&reply.model, &reply.usage));
        self.lines.push(Line {
            model: reply.model.clone(),
            usage: reply.usage,
            cost,
        });
    }

    /// Records a call that was made and could not be priced.
    ///
    /// For a reach with no price list at all, said out loud rather than arrived at by
    /// passing `None` and hoping.
    pub fn record_unpriced(&mut self, model: impl Into<ModelId>, usage: Usage) {
        self.lines.push(Line {
            model: model.into(),
            usage,
            cost: None,
        });
    }

    /// Takes everything from another ledger.
    pub fn absorb(&mut self, other: Ledger) {
        self.lines.extend(other.lines);
    }

    /// Every call, in the order they were recorded.
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// How many calls were made.
    pub fn calls(&self) -> usize {
        self.lines.len()
    }

    /// How many of them have no cost.
    ///
    /// The number that decides whether [`Ledger::total`] is a total or a floor, and the one
    /// worth printing beside it.
    pub fn unpriced(&self) -> usize {
        self.lines.iter().filter(|line| line.cost.is_none()).count()
    }

    /// Which price book editions produced the costs in here.
    ///
    /// More than one is normal over a long run and worth seeing: it means prices changed
    /// while the run was going, and the earlier calls are still recorded at what they cost
    /// then. In name order, without repeats.
    pub fn editions(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = self
            .lines
            .iter()
            .filter_map(|line| line.cost.as_ref().map(|c| c.book.as_str()))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// What the run cost.
    ///
    /// [`Total::Exact`] only when every call was priced and every price came from usage the
    /// provider reported in full. Anything else is [`Total::AtLeast`]: one unmeasured call
    /// makes the whole figure a floor, and saying otherwise is how an unknown cost becomes a
    /// free one.
    pub fn total(&self) -> Total {
        let mut amount = Micros(0);
        let mut whole = true;

        for line in &self.lines {
            match &line.cost {
                Some(priced) => {
                    amount = amount + priced.amount;
                    if priced.coverage != UsageCoverage::Exact {
                        whole = false;
                    }
                }
                None => whole = false,
            }
        }

        if whole {
            Total::Exact(amount)
        } else {
            Total::AtLeast(amount)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::message::{Message, Role, StopReason};

    fn book() -> PriceBook {
        PriceBook::parse(
            r#"
id             = "test-2026-08"
provider       = "test"
effective_from = "2026-08-01"
source         = "a fixture"
verified_at    = "2026-08-30"
currency       = "USD"

[[price]]
model  = "m"
input  = "10.00"
output = "30.00"
"#,
        )
        .unwrap_or_else(|e| panic!("the fixture book: {e}"))
    }

    fn reply(model: &str, usage: Usage) -> ChatResponse {
        ChatResponse::new(
            Message {
                role: Role::Assistant,
                content: vec![],
            },
            StopReason::EndTurn,
            usage,
            model.into(),
        )
    }

    /// Everything reported, so a price built from it is exact.
    fn measured() -> Usage {
        Usage::absent()
            .with_input(1_000_000)
            .with_cache_read(0)
            .with_cache_write(0)
            .with_output(1_000_000)
    }

    #[test]
    fn a_run_of_measured_calls_totals_exactly() {
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record(&reply("m", measured()), Some(&book()));

        let total = ledger.total();
        assert!(total.is_exact(), "{total}");
        assert_eq!(ledger.calls(), 2);
        assert_eq!(ledger.unpriced(), 0);
        // Two calls of 10 in and 30 out, per million, on a million each.
        assert_eq!(total.amount(), Micros(80_000_000));
    }

    #[test]
    fn one_unpriced_call_turns_the_whole_total_into_a_floor() {
        // The rule the type exists for. A run that mixes a priced API call with a command
        // line one must not report the first figure as though it were the bill.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record_unpriced("claude-sonnet-5", Usage::absent());

        let total = ledger.total();
        assert!(!total.is_exact(), "{total}");
        assert_eq!(
            total.amount(),
            Micros(40_000_000),
            "what is known is still known"
        );
        assert!(total.to_string().starts_with("at least"), "{total}");
    }

    #[test]
    fn a_call_nobody_could_price_is_counted_rather_than_dropped() {
        // "Forty calls, thirty of them priced" is a different sentence from "thirty calls",
        // and a ledger that reported the second would be wrong about the run.
        let mut ledger = Ledger::new();
        ledger.record_unpriced("claude-sonnet-5", Usage::absent());
        ledger.record_unpriced("claude-sonnet-5", Usage::absent());

        assert_eq!(ledger.calls(), 2);
        assert_eq!(ledger.unpriced(), 2);
        assert_eq!(ledger.total().amount(), Micros(0));
        assert!(
            !ledger.total().is_exact(),
            "nought known is not the same as nought spent"
        );
    }

    #[test]
    fn a_price_from_partial_usage_makes_the_total_a_floor_too() {
        // The cost is real and it is not the whole cost. `Priced` already carries the
        // coverage; this is that fact surviving addition.
        let mut ledger = Ledger::new();
        ledger.record(
            &reply("m", Usage::absent().with_output(1_000_000)),
            Some(&book()),
        );

        assert_eq!(ledger.unpriced(), 0, "it was priced");
        assert!(!ledger.total().is_exact(), "from half the numbers");
    }

    #[test]
    fn a_model_with_no_row_in_the_book_is_unpriced_rather_than_free() {
        let mut ledger = Ledger::new();
        ledger.record(&reply("a-model-nobody-priced", measured()), Some(&book()));

        assert_eq!(ledger.calls(), 1);
        assert_eq!(ledger.unpriced(), 1);
        assert!(!ledger.total().is_exact());
    }

    #[test]
    fn the_edition_each_cost_came_from_is_kept() {
        // So a figure can be traced back to the table that produced it, which is the whole
        // reason `Priced` carries a book id.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record(&reply("m", measured()), Some(&book()));

        assert_eq!(ledger.editions(), vec!["test-2026-08"]);
        assert_eq!(
            ledger.lines()[0].cost.as_ref().map(|c| c.book.as_str()),
            Some("test-2026-08")
        );
    }

    #[test]
    fn a_later_book_does_not_rewrite_what_an_earlier_call_cost() {
        // Prices change. What a call cost is a fact about the moment it was made, and a
        // ledger that re-priced its contents would destroy the record it exists to keep.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        let then = ledger.total().amount();

        let dearer = PriceBook::parse(
            r#"
id             = "test-2026-09"
provider       = "test"
effective_from = "2026-09-01"
source         = "a fixture"
verified_at    = "2026-09-01"
currency       = "USD"

[[price]]
model  = "m"
input  = "20.00"
output = "60.00"
"#,
        )
        .unwrap_or_else(|e| panic!("the second book: {e}"));

        ledger.record(&reply("m", measured()), Some(&dearer));

        assert_eq!(
            ledger.lines()[0].cost.as_ref().map(|c| c.amount),
            Some(then),
            "the first call still costs what it cost"
        );
        assert_eq!(ledger.editions(), vec!["test-2026-08", "test-2026-09"]);
    }

    #[test]
    fn two_ledgers_join_without_losing_what_either_knew() {
        let mut one = Ledger::new();
        one.record(&reply("m", measured()), Some(&book()));

        let mut other = Ledger::new();
        other.record_unpriced("claude-sonnet-5", Usage::absent());

        one.absorb(other);
        assert_eq!(one.calls(), 2);
        assert_eq!(one.unpriced(), 1);
        assert!(!one.total().is_exact());
    }
}
