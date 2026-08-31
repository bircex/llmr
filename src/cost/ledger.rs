//! What a run cost, and what it cannot say it cost.
//!
//! [`crate::Usage::merge`] adds two calls together. This adds up a run, and the arithmetic
//! is the easy half. The hard half is that a run almost always contains a call nobody
//! measured, and a number that quietly leaves it out is worse than no number at all.
//!
//! # The rules, and only one of them is about adding
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
//! **An unknown cost and a covered one are different facts.** A call on a plan billed by a
//! flat fee added nothing to a per-call bill; a call nobody could price might have cost
//! anything. Recording both as unpriced is what made a run of a hundred command line calls
//! report "at least 0.00" — true, and useless, on an agentic layer's main path. Say
//! [`Ledger::record_subscription`] for the first, and the total stops being a floor on
//! account of it. **The total never contains the fee**: there is no division of a
//! subscription into calls that means anything, so what a report carries is
//! [`Ledger::subscribed`] and [`Ledger::plans`] beside the figure.
//!
//! **A counted token is not a reported one.** A number this crate worked out cannot be added
//! to a number a vendor measured and come out as a measurement.
//! [`crate::UsageCoverage::Estimated`] keeps them apart and [`Total::About`] carries it
//! through, so a report can say "4.10, of which 3.80 estimated" rather than either lying
//! about the whole figure or shrugging at it. An estimate is **not** a floor: it can run
//! high, so `About` outranks `AtLeast` whenever both apply.
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
    /// Roughly this much.
    ///
    /// Part of the run was priced from tokens counted locally rather than reported, so the
    /// figure can be wrong in **either** direction. Not a floor, which is why it is not
    /// [`Total::AtLeast`]: presenting an estimate as a lower bound is a claim nobody
    /// checked, and the first time an estimate runs high the bound is simply false.
    ///
    /// [`Ledger::estimated`] says how much of the figure this covers, so a report can say
    /// "4.10, of which 3.80 estimated" rather than shrugging at the whole number.
    About(Micros),
}

impl Total {
    /// The number, whichever kind it is.
    ///
    /// Read [`Total::is_exact`] before you present it as a bill.
    pub fn amount(self) -> Micros {
        match self {
            Total::Exact(amount) | Total::AtLeast(amount) | Total::About(amount) => amount,
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
            Total::About(amount) => write!(f, "about {amount}"),
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
    /// The plan this call was covered by, when it was not billed per call.
    ///
    /// `Some` means the caller said out loud that this route is paid for by a flat fee, so
    /// the call added nothing to a per-call bill. That is a different fact from an unknown
    /// cost and the ledger keeps them apart: an unknown cost makes a total a floor, a
    /// covered one does not.
    ///
    /// **The total never includes the fee.** A subscription is not a per-call cost and
    /// there is no way to divide one into calls that means anything, so what a report says
    /// is the number of calls and which plan, and the person reading it knows what they pay.
    pub subscription: Option<String>,
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
            subscription: None,
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
            subscription: None,
        });
    }

    /// Records a call covered by a flat fee rather than billed per call.
    ///
    /// The case is a subscription command line tool. It reports no usage and has no price
    /// row, so [`Ledger::record_unpriced`] is what it gets today, and a bot making a hundred
    /// of them is told its run cost "at least 0.00": true, and useless, on the path it spends
    /// most of its life.
    ///
    /// Saying `record_subscription` instead moves the call out of the unknown column and
    /// into a named one. [`Ledger::total`] stops being a floor on account of it,
    /// [`Ledger::subscribed`] counts it, and [`Ledger::plans`] names what covers it.
    ///
    /// ```
    /// use llmr::cost::ledger::Ledger;
    /// # use llmr::Usage;
    /// let mut ledger = Ledger::new();
    /// ledger.record_subscription("claude-sonnet-5", "claude-max", Usage::absent());
    ///
    /// assert_eq!(ledger.calls(), 1);
    /// assert_eq!(ledger.unpriced(), 0, "covered is not unknown");
    /// assert_eq!(ledger.subscribed(), 1);
    /// assert_eq!(ledger.plans(), vec!["claude-max"]);
    /// ```
    ///
    /// # This is a claim, and only the caller can make it
    ///
    /// Nothing about a command line tool says how the account behind it is billed. The same
    /// program signed in one way is a flat fee and signed in another is metered per token,
    /// and this crate cannot tell which. So no preset sets it, [`crate::Provider`] answers
    /// `None` until somebody says otherwise, and calling this is that somebody saying so.
    ///
    /// Getting it wrong writes a metered call down as covered, which is the zero this whole
    /// module exists to prevent, wearing a better name. The protection is that it cannot
    /// happen by accident: nothing reaches this method without a plan name being typed.
    pub fn record_subscription(
        &mut self,
        model: impl Into<ModelId>,
        plan: impl Into<String>,
        usage: Usage,
    ) {
        self.lines.push(Line {
            model: model.into(),
            usage,
            cost: None,
            subscription: Some(plan.into()),
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

    /// Records a reply, asking the provider how it is billed.
    ///
    /// The version of [`Ledger::record`] for a program holding an `Arc<dyn Provider>` and a
    /// mixture of metered and covered routes. A provider that answers
    /// [`crate::Provider::subscription`] gets its call recorded as covered; every other one
    /// is priced against the book exactly as [`Ledger::record`] would.
    ///
    /// One call site rather than a `match` at each of them, so a route added later cannot be
    /// recorded the wrong way in one place and the right way in another.
    pub fn record_from(
        &mut self,
        provider: &dyn crate::provider::Provider,
        reply: &ChatResponse,
        book: Option<&PriceBook>,
    ) {
        match provider.subscription() {
            Some(plan) => self.record_subscription(reply.model.clone(), plan, reply.usage),
            None => self.record(reply, book),
        }
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

    /// How many of them have no cost, and nothing said why.
    ///
    /// The number that decides whether [`Ledger::total`] is a total or a floor, and the one
    /// worth printing beside it.
    ///
    /// A call covered by a subscription is **not** here. Its cost is not unknown, it is out
    /// of scope, and folding the two together is what makes a whole run of command line
    /// calls report as "at least 0.00". [`Ledger::subscribed`] counts those instead.
    pub fn unpriced(&self) -> usize {
        self.lines
            .iter()
            .filter(|line| line.cost.is_none() && line.subscription.is_none())
            .count()
    }

    /// How many calls were covered by a flat fee rather than billed per call.
    ///
    /// Belongs beside [`Ledger::total`] in any report. A total of nothing over forty covered
    /// calls is a correct sentence; the same total with no count beside it is a bill nobody
    /// should believe.
    pub fn subscribed(&self) -> usize {
        self.lines
            .iter()
            .filter(|line| line.subscription.is_some())
            .count()
    }

    /// Which plans covered the calls in here. In name order, without repeats.
    pub fn plans(&self) -> Vec<&str> {
        let mut seen: Vec<&str> = self
            .lines
            .iter()
            .filter_map(|line| line.subscription.as_deref())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    }

    /// How much of a total rests on tokens counted here rather than reported.
    ///
    /// One figure per currency, in code order, so a report can say "4.10, of which 3.80
    /// estimated". Empty when nothing in the run was estimated.
    ///
    /// This is the whole reason [`crate::UsageCoverage::Estimated`] is a variant rather than
    /// a fold into `Exact`: without it the estimated part of a bill is unfindable once it
    /// has been added to the measured part.
    pub fn estimated(&self) -> Vec<(String, Micros)> {
        let mut sums: BTreeMap<&str, Micros> = BTreeMap::new();
        for priced in self
            .lines
            .iter()
            .filter_map(|line| line.cost.as_ref())
            .filter(|priced| priced.coverage == UsageCoverage::Estimated)
        {
            let entry = sums.entry(priced.currency.as_str()).or_insert(Micros(0));
            *entry = *entry + priced.amount;
        }
        sums.into_iter()
            .map(|(currency, amount)| (currency.to_string(), amount))
            .collect()
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
    /// [`Total::Exact`] only when every call was priced from usage the provider reported in
    /// full, or covered by a plan the caller named. Otherwise:
    ///
    /// | In the run | Total |
    /// |---|---|
    /// | anything estimated | [`Total::About`] |
    /// | anything unpriced, or priced from partial usage | [`Total::AtLeast`] |
    /// | neither | [`Total::Exact`] |
    ///
    /// **An estimate outranks a floor when both are true**, and that is not the obvious
    /// order. A floor claims the real figure is this or more. An estimate can run high, so
    /// one estimate anywhere in the run makes that claim unsafe, and a lower bound that can
    /// be false is worse than an honest approximation. `About` says less and is true.
    ///
    /// It says less about the unpriced calls too, which is why [`Ledger::unpriced`],
    /// [`Ledger::estimated`] and [`Ledger::subscribed`] belong beside it in any report. One
    /// enum cannot carry three facts, and [`Ledger::summary`] is the version that says them
    /// all in a sentence.
    ///
    /// `None` when the priced calls are in more than one currency. There is no exchange rate
    /// in this crate and there should not be one: a rate has a date and a source, exactly
    /// like a price, and inventing one to make this method return a number would produce a
    /// figure nobody could audit. Ask [`Ledger::totals`] instead.
    pub fn total(&self) -> Option<Total> {
        if self.currencies().len() > 1 {
            return None;
        }

        let mut amount = Micros(0);
        for priced in self.lines.iter().filter_map(|line| line.cost.as_ref()) {
            amount = amount + priced.amount;
        }
        Some(self.kind(
            amount,
            self.unpriced() > 0,
            self.lines.iter().filter_map(|l| l.cost.as_ref()),
        ))
    }

    /// Which of the three a figure is, given what went into it.
    ///
    /// One place, so [`Ledger::total`] and [`Ledger::totals`] cannot come to different
    /// conclusions about the same run.
    fn kind<'a>(
        &self,
        amount: Micros,
        any_unpriced: bool,
        priced: impl Iterator<Item = &'a Priced>,
    ) -> Total {
        let mut estimated = false;
        let mut understated = any_unpriced;
        for cost in priced {
            match cost.coverage {
                UsageCoverage::Estimated => estimated = true,
                UsageCoverage::Exact => {}
                // Absent never prices, so this is Partial: some of what was billed was
                // never reported, and the figure is short by whatever it was.
                _ => understated = true,
            }
        }

        if estimated {
            Total::About(amount)
        } else if understated {
            Total::AtLeast(amount)
        } else {
            Total::Exact(amount)
        }
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
        let mut sums: BTreeMap<&str, Micros> = BTreeMap::new();

        for priced in self.lines.iter().filter_map(|line| line.cost.as_ref()) {
            let entry = sums.entry(priced.currency.as_str()).or_insert(Micros(0));
            *entry = *entry + priced.amount;
        }

        sums.into_iter()
            .map(|(currency, amount)| {
                let here = self
                    .lines
                    .iter()
                    .filter_map(|line| line.cost.as_ref())
                    .filter(|priced| priced.currency == currency);
                (currency.to_string(), self.kind(amount, any_unpriced, here))
            })
            .collect()
    }

    /// The run in one sentence, with nothing left out.
    ///
    /// The answer to "what did that cost", written so that the parts a person has to act on
    /// differently are named separately: what was measured, what was estimated, what nobody
    /// could price, and what a flat fee covers.
    ///
    /// ```
    /// use llmr::cost::ledger::Ledger;
    /// # use llmr::Usage;
    /// let mut ledger = Ledger::new();
    /// ledger.record_subscription("claude-sonnet-5", "claude-max", Usage::absent());
    /// ledger.record_subscription("claude-sonnet-5", "claude-max", Usage::absent());
    ///
    /// assert_eq!(
    ///     ledger.summary(),
    ///     "2 calls, nothing billed per call, 2 covered by claude-max"
    /// );
    /// ```
    ///
    /// A sentence rather than a struct because this is the thing a person reads, and every
    /// program that assembled it from [`Ledger::total`], [`Ledger::unpriced`] and the rest
    /// would leave one of them out. The pieces are all still there for a program that wants
    /// to render its own.
    pub fn summary(&self) -> String {
        if self.lines.is_empty() {
            return "no calls".into();
        }

        let mut out = format!("{} calls", self.calls());

        let totals = self.totals();
        if totals.is_empty() {
            out.push_str(", nothing billed per call");
        } else {
            let figures: Vec<String> = totals
                .iter()
                .map(|(currency, total)| format!("{total} {currency}"))
                .collect();
            out.push_str(&format!(", {}", figures.join(" + ")));

            let estimated: Vec<String> = self
                .estimated()
                .into_iter()
                .map(|(currency, amount)| format!("{amount} {currency}"))
                .collect();
            if !estimated.is_empty() {
                out.push_str(&format!(", of which {} estimated", estimated.join(" + ")));
            }
        }

        if self.unpriced() > 0 {
            out.push_str(&format!(", {} with no figure at all", self.unpriced()));
        }
        if self.subscribed() > 0 {
            out.push_str(&format!(
                ", {} covered by {}",
                self.subscribed(),
                self.plans().join(" and ")
            ));
        }
        out
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

    /// The same numbers, counted here rather than reported.
    fn counted() -> Usage {
        measured().estimating()
    }

    #[test]
    fn a_run_of_covered_calls_is_out_of_scope_rather_than_unknown() {
        // The failure this fixes. A hundred command line calls used to report "at least
        // 0.00", which is true, useless, and the crate's main path.
        let mut ledger = Ledger::new();
        for _ in 0..3 {
            ledger.record_subscription("claude-sonnet-5", "claude-max", Usage::absent());
        }

        assert_eq!(ledger.calls(), 3);
        assert_eq!(ledger.unpriced(), 0, "covered is not unknown");
        assert_eq!(ledger.subscribed(), 3);
        assert_eq!(ledger.plans(), vec!["claude-max"]);
        assert_eq!(
            ledger.summary(),
            "3 calls, nothing billed per call, 3 covered by claude-max"
        );
    }

    #[test]
    fn a_covered_call_does_not_turn_a_measured_run_into_a_floor() {
        // A bot that calls an API and a subscription tool in the same run. The API half is
        // measured and priced, and the covered half adds nothing to a per-call bill, so
        // there is nothing about the total that is unknown.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record_subscription("claude-sonnet-5", "claude-max", Usage::absent());

        let total = sum(&ledger);
        assert!(total.is_exact(), "{total}");
        assert_eq!(total.amount(), Micros(40_000_000));
    }

    #[test]
    fn a_covered_call_never_adds_a_fee_to_the_total() {
        // The subscription is not divided into calls, because there is no division of it
        // that means anything. What a report says is the count and the plan.
        let mut ledger = Ledger::new();
        ledger.record_subscription("m", "claude-max", Usage::absent());
        assert_eq!(sum(&ledger).amount(), Micros(0));
        assert!(
            ledger.summary().contains("covered by claude-max"),
            "{}",
            ledger.summary()
        );
    }

    #[test]
    fn an_estimated_call_is_about_rather_than_exact() {
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", counted()), Some(&book()));

        let total = sum(&ledger);
        assert!(!total.is_exact(), "{total}");
        assert_eq!(total, Total::About(Micros(40_000_000)));
        assert!(total.to_string().starts_with("about"), "{total}");
        assert_eq!(ledger.unpriced(), 0, "an estimate is a figure, not a gap");
    }

    #[test]
    fn an_estimate_outranks_a_floor_when_both_are_true() {
        // Not the obvious order, and the reason is that a floor can be false. An estimate
        // can run high, so "at least 40.00" is a claim nobody checked the moment one
        // estimated line is in the sum. `About` says less and is true.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", counted()), Some(&book()));
        ledger.record_unpriced("m", Usage::absent());

        assert!(matches!(sum(&ledger), Total::About(_)), "{}", sum(&ledger));
        assert_eq!(
            ledger.unpriced(),
            1,
            "and the call with no figure is still findable"
        );
    }

    #[test]
    fn how_much_of_a_bill_was_estimated_stays_findable_after_it_is_added_up() {
        // The whole reason `Estimated` is a variant rather than a fold into `Exact`.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record(&reply("m", counted()), Some(&book()));

        assert_eq!(sum(&ledger).amount(), Micros(80_000_000));
        assert_eq!(
            ledger.estimated(),
            vec![("USD".into(), Micros(40_000_000))],
            "half of it was counted here rather than reported"
        );
        assert_eq!(
            ledger.summary(),
            "2 calls, about 80.000000 USD, of which 40.000000 USD estimated"
        );
    }

    #[test]
    fn a_run_that_is_all_three_says_all_three() {
        // What an agentic run actually looks like: a measured API call, a locally counted
        // one, a call nobody could price, and a covered command line call. One sentence has
        // to carry every part, because a person acts differently on each.
        let mut ledger = Ledger::new();
        ledger.record(&reply("m", measured()), Some(&book()));
        ledger.record(&reply("m", counted()), Some(&book()));
        ledger.record_unpriced("m", Usage::absent());
        ledger.record_subscription("m", "claude-max", Usage::absent());

        assert_eq!(
            ledger.summary(),
            "4 calls, about 80.000000 USD, of which 40.000000 USD estimated, \
             1 with no figure at all, 1 covered by claude-max"
        );
    }

    #[test]
    fn an_empty_ledger_says_so_rather_than_reporting_nothing() {
        assert_eq!(Ledger::new().summary(), "no calls");
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
