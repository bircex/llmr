//! A cap on what a run may spend, checked before the money goes.
//!
//! [`crate::Ledger`] records what a run cost. Nothing stopped it. An agentic bot spends
//! money with nobody watching, and the failure this prevents is not theoretical: a loop that
//! retries, or a plan that expands, and nobody finds out until the invoice.
//!
//! ```
//! # use llmr::{Budget, Micros, Router};
//! # fn example(routes: Vec<llmr::Route>) -> Result<(), String> {
//! let budget = Budget::of(Micros::parse("5.00")?, "USD");
//! let router = Router::new(routes).within(budget);
//! # Ok(())
//! # }
//! ```
//!
//! # Three things it has to get right
//!
//! **It refuses before spending, not after.** A cap checked after the call is a report. So
//! the check happens before a request goes out, and what it can check before is stated below
//! rather than implied.
//!
//! **An unpriced call cannot be checked at all.** A route with no price book cannot be
//! measured against a cap, and pretending otherwise is worse than admitting it. Under a
//! budget such a route is refused by default, and [`Budget::allowing_unpriced`] is how a
//! caller says otherwise, out loud, and gets a [`Spending::unmeasured`] count back to prove
//! the figure is a floor.
//!
//! **A budget is in one currency.** [`crate::Ledger::total`] already answers `None` for a
//! mixed run because there is no exchange rate in this crate and there should not be one: a
//! rate has a date and a source exactly like a price does. A route priced in another
//! currency is refused under a budget for the same reason, rather than converted.
//!
//! # What can be checked before a call, and what cannot
//!
//! Two things, and both are real numbers rather than guesses.
//!
//! * **Is anything left.** A budget that is spent refuses everything.
//! * **Could the reply alone overrun it.** When a request sets a maximum reply length with
//!   [`ChatRequest::with_max_tokens`](crate::ChatRequest::with_max_tokens), the most the
//!   output can cost is that many tokens at the route's output rate. If that exceeds what is
//!   left, the call cannot fit however short the prompt turns out to be.
//!
//! What cannot be checked is the prompt, because pricing it needs a token count and this
//! crate does not count tokens. That decision is written down in `docs/DESIGN.md` and it is
//! the same one everywhere: a tokeniser that is close produces numbers that look right and
//! are not.
//!
//! So a budget is a cap on what a run is **allowed to start**, and the last call in a run can
//! carry it over. Set `max_tokens` and the overshoot is bounded by the rate you can read.

use crate::cost::pricing::{Micros, Rate};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

/// What to do with a route whose cost cannot be worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Unpriced {
    /// Never used while a budget is set. The default.
    ///
    /// A cap that cannot be checked is not a cap. If the point of the budget is to promise a
    /// bot spends at most five pounds, a call nobody can price is exactly the thing that
    /// breaks the promise without anything noticing.
    #[default]
    Refused,
    /// Used, and counted as one call whose cost is unknown.
    ///
    /// The right answer when the unpriced routes are subscription command line tools, whose
    /// calls genuinely add nothing to a per-call bill. [`Spending::unmeasured`] counts them,
    /// so [`Spending::spent`] can be read as the floor it is.
    Allowed,
}

/// A cap on what a run may spend.
///
/// Immutable, like every other policy here. What changes is the [`crate::Router`]'s running
/// total, which is atomics for the reason the circuit breaker's are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    cap: Micros,
    currency: String,
    unpriced: Unpriced,
}

impl Budget {
    /// This much, in this currency.
    ///
    /// The currency is not optional and there is no default. [`Micros`] is a bare integer
    /// and two of them add whether or not they are the same money, so a budget that did not
    /// name its currency would happily be spent in another one.
    pub fn of(cap: Micros, currency: impl Into<String>) -> Self {
        Self {
            cap,
            currency: currency.into(),
            unpriced: Unpriced::Refused,
        }
    }

    /// Let a route nobody can price run anyway, counted as unmeasured.
    ///
    /// Said out loud rather than arrived at. Without this a budget refuses what it cannot
    /// check, which is the only setting under which "this run spent at most the cap" is a
    /// sentence anybody should believe.
    #[must_use]
    pub fn allowing_unpriced(mut self) -> Self {
        self.unpriced = Unpriced::Allowed;
        self
    }

    /// The cap.
    pub fn cap(&self) -> Micros {
        self.cap
    }

    /// Which money the cap is in, as an ISO code such as `USD`.
    pub fn currency(&self) -> &str {
        &self.currency
    }

    /// What this budget does with a route it cannot price.
    pub fn unpriced(&self) -> Unpriced {
        self.unpriced
    }

    /// The most a reply of this length could cost at this rate.
    ///
    /// An upper bound rather than an estimate, which is why it is safe to refuse on. Every
    /// token is counted at the output rate, and no reply is longer than the limit it was
    /// given.
    pub fn most_a_reply_can_cost(max_tokens: u32, rate: &Rate) -> Micros {
        Micros(i64::from(max_tokens).saturating_mul(rate.output.0) / 1_000_000)
    }
}

/// What a budget has been spent on so far.
///
/// A snapshot. Ask again after a call and the numbers will have moved.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Spending {
    /// The cap this run was given.
    pub cap: Micros,
    /// What has been priced against it.
    ///
    /// A floor rather than a total whenever [`Spending::unmeasured`] is not zero, for the
    /// same reason [`crate::cost::Total::AtLeast`] exists.
    pub spent: Micros,
    /// What is left, never below zero.
    pub remaining: Micros,
    /// Which money all of the above is in.
    pub currency: String,
    /// Calls that were made and could not be priced.
    ///
    /// Zero unless the budget was built with [`Budget::allowing_unpriced`], or a streamed
    /// call was routed: a stream reports its usage into a
    /// [`Transcript`](crate::Transcript) the router never sees. Add one with
    /// [`crate::Router::charge`] when you have it.
    pub unmeasured: usize,
}

impl Spending {
    /// Whether [`Spending::spent`] is the whole of what this run has cost.
    ///
    /// `false` when anything was made that could not be priced, which makes the figure a
    /// floor. Read it before presenting the number as a bill, exactly as with
    /// [`crate::cost::Total::is_exact`].
    pub fn is_exact(&self) -> bool {
        self.unmeasured == 0
    }
}

/// The running total behind a [`Budget`].
///
/// Atomics, so [`crate::Router::chat`] can keep taking `&self` and one router stays shared
/// across as many tasks as a program has. Two tasks charging at once add their two amounts;
/// neither is lost.
///
/// The race that is *not* prevented is two tasks both passing the check with room for one
/// call and both spending it. Preventing that needs a reservation held across the call, which
/// means a lock held across an await, which this crate forbids for a much worse reason than
/// this one. So a budget is a cap on what a run may **start**, and a run with many tasks in
/// flight can overshoot by the calls that were already going.
#[derive(Debug, Default)]
pub(crate) struct Purse {
    spent: AtomicI64,
    unmeasured: AtomicUsize,
}

impl Purse {
    /// What is left of this cap, never below zero.
    pub(crate) fn remaining(&self, cap: Micros) -> Micros {
        Micros(
            cap.0
                .saturating_sub(self.spent.load(Ordering::Relaxed))
                .max(0),
        )
    }

    /// Adds to the running total.
    pub(crate) fn spend(&self, amount: Micros) {
        self.spent.fetch_add(amount.0, Ordering::Relaxed);
    }

    /// Records a call that happened and could not be priced.
    pub(crate) fn unmeasured(&self) {
        self.unmeasured.fetch_add(1, Ordering::Relaxed);
    }

    /// Takes one back off the unmeasured count, for a call that has since been priced.
    pub(crate) fn measured_after_all(&self) {
        let _ = self
            .unmeasured
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
    }

    /// The snapshot a caller reads.
    pub(crate) fn spending(&self, budget: &Budget) -> Spending {
        Spending {
            cap: budget.cap,
            spent: Micros(self.spent.load(Ordering::Relaxed)),
            remaining: self.remaining(budget.cap),
            currency: budget.currency.clone(),
            unmeasured: self.unmeasured.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn five() -> Budget {
        Budget::of(Micros(5_000_000), "USD")
    }

    #[test]
    fn a_budget_refuses_what_it_cannot_check_unless_told_otherwise() {
        // A cap that cannot be checked is not a cap, and this is the only setting under
        // which "this run spent at most five dollars" is a sentence anybody should believe.
        assert_eq!(five().unpriced(), Unpriced::Refused);
        assert_eq!(five().allowing_unpriced().unpriced(), Unpriced::Allowed);
    }

    #[test]
    fn what_is_left_never_goes_below_zero() {
        // A negative remainder would compare as "room for a small call" in exactly the
        // arithmetic that is supposed to stop one.
        let purse = Purse::default();
        purse.spend(Micros(9_000_000));
        assert_eq!(purse.remaining(Micros(5_000_000)), Micros(0));
    }

    #[test]
    fn the_most_a_reply_can_cost_is_an_upper_bound_rather_than_a_guess() {
        // Safe to refuse on, because no reply is longer than the limit it was given and
        // every token is counted at the dearest of the four rates it could be billed at.
        let rate = Rate {
            output: Micros(15_000_000),
            ..Rate::default()
        };
        assert_eq!(
            Budget::most_a_reply_can_cost(1_000_000, &rate),
            Micros(15_000_000)
        );
        assert_eq!(Budget::most_a_reply_can_cost(1_000, &rate), Micros(15_000));
    }

    #[test]
    fn spending_says_whether_it_is_the_whole_figure() {
        let purse = Purse::default();
        purse.spend(Micros(1_000_000));
        assert!(purse.spending(&five()).is_exact());

        purse.unmeasured();
        let spending = purse.spending(&five());
        assert!(!spending.is_exact());
        assert_eq!(spending.spent, Micros(1_000_000), "what is known is known");
        assert_eq!(spending.remaining, Micros(4_000_000));
    }

    #[test]
    fn a_call_priced_later_stops_counting_as_unmeasured() {
        // A streamed call whose transcript the caller has since read.
        let purse = Purse::default();
        purse.unmeasured();
        purse.measured_after_all();
        assert!(purse.spending(&five()).is_exact());

        // And the count never wraps past zero, which would read as an enormous number of
        // unmeasured calls on a run that had none.
        purse.measured_after_all();
        assert_eq!(purse.spending(&five()).unmeasured, 0);
    }
}
