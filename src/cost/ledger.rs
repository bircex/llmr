//! What a run cost, and what it cannot say it cost.
//!
//! [`crate::Usage::merge`] adds two calls together. This adds up a run, and the arithmetic
//! is the easy half. The hard half is that a run almost always contains a call nobody
//! measured, and a number that quietly leaves it out is worse than no number at all.
//!
//! # Four rules, and only one of them is about adding
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
//! **A call whose reply was never read still happened.** Drop the losing future of a hedged
//! pair and the request went out and will be billed, but no reply came back, so nothing
//! calls [`Ledger::record`] and the ledger says the run was one measured call. Say
//! [`Ledger::record_cancelled`] instead. This is the rule that is easiest to miss, because
//! nothing in the code that dropped the future looks like a cost.
//!
//! **A sum needs one currency.** [`Micros`] is an integer, and two of them add whether or not
//! they are the same money. So [`Ledger::total`] answers `None` when the run was priced in
//! more than one, and [`Ledger::totals`] gives one figure per currency instead. A number that
//! is part dollars and part euros is worse than no number, because it looks like a number.
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
//!
//! // Nothing was priced, so nothing disagrees about the currency and there is a total.
//! let total = ledger.total().unwrap_or_else(|| unreachable!("one currency at most"));
//! assert!(!total.is_exact(), "one unpriced call makes it a floor");
//! assert_eq!(ledger.currency(), None);
//! ```

use crate::chat::response::ChatResponse;
use crate::cost::pricing::{Micros, PriceBook, Priced};
use crate::cost::usage::{Usage, UsageCoverage};
use crate::model::ModelId;
use std::collections::BTreeMap;

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
    ///
    /// Not the method for a call whose reply was never read. That is
    /// [`Ledger::record_cancelled`], which does the same thing under a name that says what
    /// happened.
    pub fn record_unpriced(&mut self, model: impl Into<ModelId>, usage: Usage) {
        self.lines.push(Line {
            model: model.into(),
            usage,
            cost: None,
        });
    }

    /// Records a call that went out and whose reply was never read.
    ///
    /// The case this exists for is hedging: two providers are asked the same question, the
    /// first answer wins, and the loser's future is dropped. That request was sent and it
    /// will be billed. No [`ChatResponse`] ever came back, so there is no usage to record
    /// and [`Ledger::record`] is never called for it.
    ///
    /// Without this line the ledger holds one call, that call was measured, and
    /// [`Ledger::total`] answers [`Total::Exact`]. A confident figure that is wrong by
    /// whatever the losing call cost, which is the one failure this whole module exists to
    /// prevent.
    ///
    /// ```
    /// use llmr::cost::ledger::Ledger;
    /// let mut ledger = Ledger::new();
    /// ledger.record_cancelled("claude-sonnet-5");
    ///
    /// assert_eq!(ledger.calls(), 1);
    /// assert_eq!(ledger.unpriced(), 1);
    /// ```
    ///
    /// Recorded as [`Usage::absent`], never as zero, for the same reason everything else
    /// unmeasured is. The distinction from [`Ledger::record_unpriced`] is only in the name:
    /// that one is a reach with no price list, this one is a reply nobody read. They cost
    /// the ledger the same thing and they are different mistakes to make, so a report that
    /// says which is a report somebody can act on.
    pub fn record_cancelled(&mut self, model: impl Into<ModelId>) {
        self.record_unpriced(model, Usage::absent());
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

    /// Which currencies the costs in here are in. In code order, without repeats.
    ///
    /// Empty when nothing was priced.
    pub fn currencies(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = self
            .lines
            .iter()
            .filter_map(|line| line.cost.as_ref().map(|c| c.currency.as_str()))
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// The one currency this run was priced in, if there is one.
    ///
    /// `None` for two reasons that a caller has to tell apart, and [`Ledger::currencies`] is
    /// how: nothing was priced at all, or the run mixes currencies and no single figure
    /// exists.
    pub fn currency(&self) -> Option<&str> {
        match self.currencies().as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// What the run cost.
    ///
    /// [`Total::Exact`] only when every call was priced and every price came from usage the
    /// provider reported in full. Anything else is [`Total::AtLeast`]: one unmeasured call
    /// makes the whole figure a floor, and saying otherwise is how an unknown cost becomes a
    /// free one.
    ///
    /// `None` when the priced calls are in more than one currency. There is no exchange rate
    /// in this crate and there should not be one — a rate has a date and a source, exactly
    /// like a price, and inventing one to make this method return a number would produce a
    /// figure nobody could audit. Ask [`Ledger::totals`] instead.
    pub fn total(&self) -> Option<Total> {
        if self.currencies().len() > 1 {
            return None;
        }

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

        Some(if whole {
            Total::Exact(amount)
        } else {
            Total::AtLeast(amount)
        })
    }

    /// What the run cost, one figure per currency, in code order.
    ///
    /// The answer when [`Ledger::total`] says `None`, and the same figure it would have given
    /// when it says anything else.
    ///
    /// An unpriced call makes *every* line here a floor, not just one: nothing says which
    /// currency it would have been in. And a run of nothing but unpriced calls comes back
    /// empty, which is why [`Ledger::calls`] and [`Ledger::unpriced`] belong beside it in any
    /// report — an empty list is not a run that cost nothing.
    pub fn totals(&self) -> Vec<(String, Total)> {
        let any_unpriced = self.unpriced() > 0;
        let mut sums: BTreeMap<&str, (Micros, bool)> = BTreeMap::new();

        for priced in self.lines.iter().filter_map(|line| line.cost.as_ref()) {
            let entry = sums
                .entry(priced.currency.as_str())
                .or_insert((Micros(0), !any_unpriced));
            entry.0 = entry.0 + priced.amount;
            if priced.coverage != UsageCoverage::Exact {
                entry.1 = false;
            }
        }

        sums.into_iter()
            .map(|(currency, (amount, whole))| {
                (
                    currency.to_string(),
                    if whole {
                        Total::Exact(amount)
                    } else {
                        Total::AtLeast(amount)
                    },
                )
            })
            .collect()
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

    /// The same rates, billed in a different currency.
    fn in_euros() -> PriceBook {
        PriceBook::parse(
            r#"
id             = "test-eur-2026-08"
provider       = "test"
effective_from = "2026-08-01"
source         = "a fixture"
verified_at    = "2026-08-30"
currency       = "EUR"

[[price]]
model  = "m"
input  = "10.00"
output = "30.00"
"#,
        )
        .unwrap_or_else(|e| panic!("the euro book: {e}"))
    }

    /// The total of a run that is in one currency, which is what most of these tests are.
    fn sum(ledger: &Ledger) -> Total {
        match ledger.total() {
            Some(total) => total,
            None => panic!("this run is priced in one currency"),
        }
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

        let total = sum(&ledger);
        assert!(total.is_exact(), "{total}");
        assert_eq!(ledger.calls(), 2);
        assert_eq!(ledger.unpriced(), 0);
        // Two calls of 10 in and 30 out, per million, on a million each.
        assert_eq!(total.amount(), Micros(80_000_000));
    }

    #[test]
    fn the_loser_of_a_hedged_pair_makes_the_total_a_floor() {
        // Two providers asked the same question, the first answer taken, the other future
        // dropped. Without the second line this ledger reports one measured call and an
        // exact total, having been billed for two.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record_cancelled("m");

        assert_eq!(ledger.calls(), 2, "both requests were sent");
        assert_eq!(ledger.unpriced(), 1);

        let total = sum(&ledger);
        assert!(!total.is_exact(), "{total}");
        assert_eq!(
            total.amount(),
            Micros(40_000_000),
            "the winner is still priced"
        );
    }

    #[test]
    fn one_unpriced_call_turns_the_whole_total_into_a_floor() {
        // The rule the type exists for. A run that mixes a priced API call with a command
        // line one must not report the first figure as though it were the bill.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record_unpriced("claude-sonnet-5", Usage::absent());

        let total = sum(&ledger);
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
        assert_eq!(sum(&ledger).amount(), Micros(0));
        assert!(
            !sum(&ledger).is_exact(),
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
        assert!(!sum(&ledger).is_exact(), "from half the numbers");
    }

    #[test]
    fn a_model_with_no_row_in_the_book_is_unpriced_rather_than_free() {
        let mut ledger = Ledger::new();
        ledger.record(&reply("a-model-nobody-priced", measured()), Some(&book()));

        assert_eq!(ledger.calls(), 1);
        assert_eq!(ledger.unpriced(), 1);
        assert!(!sum(&ledger).is_exact());
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
        let then = sum(&ledger).amount();

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
    fn a_run_in_one_currency_says_which_one() {
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));

        assert_eq!(ledger.currency(), Some("USD"));
        assert_eq!(
            ledger.totals(),
            vec![("USD".to_string(), Total::Exact(Micros(40_000_000)))]
        );
    }

    #[test]
    fn a_run_priced_in_two_currencies_has_no_single_total() {
        // The hole this all exists to close. `Micros` is an integer: forty dollars and forty
        // euros add to eighty of nothing, and the answer looks exactly like a real one.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record(&reply("m", measured()), Some(&in_euros()));

        assert_eq!(ledger.total(), None, "there is no such number");
        assert_eq!(ledger.currency(), None);
        assert_eq!(ledger.currencies(), vec!["EUR", "USD"]);
        assert_eq!(ledger.calls(), 2, "both calls still happened");
    }

    #[test]
    fn a_mixed_run_is_totalled_one_currency_at_a_time() {
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record(&reply("m", measured()), Some(&in_euros()));
        ledger.record(&reply("m", measured()), Some(&in_euros()));

        assert_eq!(
            ledger.totals(),
            vec![
                ("EUR".to_string(), Total::Exact(Micros(80_000_000))),
                ("USD".to_string(), Total::Exact(Micros(40_000_000))),
            ]
        );
    }

    #[test]
    fn one_unpriced_call_makes_every_currency_a_floor() {
        // Not just one of them. Nothing records which currency the unpriced call would have
        // been billed in, so it is missing from all of them.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record(&reply("m", measured()), Some(&in_euros()));
        ledger.record_unpriced("claude-sonnet-5", Usage::absent());

        assert_eq!(
            ledger.totals(),
            vec![
                ("EUR".to_string(), Total::AtLeast(Micros(40_000_000))),
                ("USD".to_string(), Total::AtLeast(Micros(40_000_000))),
            ]
        );
    }

    #[test]
    fn a_run_nobody_priced_totals_to_a_floor_rather_than_to_nothing() {
        // No currency disagrees with any other, because none was named. That is a total, and
        // an empty `totals()` beside a call count of two is the shape a report has to print.
        let mut ledger = Ledger::new();
        ledger.record_unpriced("claude-sonnet-5", Usage::absent());
        ledger.record_unpriced("claude-sonnet-5", Usage::absent());

        assert_eq!(ledger.total(), Some(Total::AtLeast(Micros(0))));
        assert_eq!(ledger.currency(), None);
        assert!(ledger.totals().is_empty());
        assert_eq!(ledger.calls(), 2);
    }

    #[test]
    fn absorbing_a_ledger_in_another_currency_is_noticed() {
        // Two tasks, two books, joined at the end. This is the realistic way a mixed run
        // happens, and the join must not produce a number.
        let mut one = Ledger::new();
        one.record(&reply("m", measured()), Some(&book()));
        assert!(one.total().is_some());

        let mut other = Ledger::new();
        other.record(&reply("m", measured()), Some(&in_euros()));

        one.absorb(other);
        assert_eq!(one.total(), None);
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
        assert!(!sum(&one).is_exact());
    }
}
