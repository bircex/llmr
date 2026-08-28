//! The tables that ship with this crate, read the way a caller reads them.
//!
//! They are data in a file, so they can be wrong in a commit nobody looked at closely. A
//! missing date, a model priced twice, a row with no source: every one of those parses fine
//! as TOML and is refused here.

#![cfg(feature = "anthropic")]

use llmr::providers::anthropic::{shipped_prices, shipped_registry};
use llmr::{Micros, ModelId, Reach, Usage, UsageCoverage};

#[test]
fn the_shipped_registry_reads_and_knows_the_models_it_claims() {
    let registry = shipped_registry();
    assert!(!registry.ids().is_empty(), "the table failed to parse");
    assert_eq!(registry.reach, Reach::FirstPartyApi);

    let caps = registry.capabilities(&ModelId::from("claude-sonnet-5"));
    assert!(
        caps.is_some(),
        "a model this crate names is not in its table"
    );
    assert!(caps.map(|c| c.context_window > 0).unwrap_or(false));
}

#[test]
fn the_shipped_prices_read_and_carry_their_provenance() {
    let prices = shipped_prices();
    assert!(!prices.rates.is_empty(), "the price file failed to parse");
    assert!(!prices.verified_at.is_empty(), "a price with no date");
    assert!(!prices.source.is_empty(), "a price with no source");
    assert_eq!(prices.currency, "USD");
}

#[test]
fn every_priced_model_is_one_the_registry_knows() {
    // A price for a model nobody can reach is a row that will never be used, and a sign
    // that one of the two files was edited without the other.
    let registry = shipped_registry();
    let prices = shipped_prices();

    let unknown: Vec<&String> = prices
        .rates
        .keys()
        .filter(|model| {
            registry
                .capabilities(&ModelId::from(model.as_str()))
                .is_none()
        })
        .collect();

    assert!(
        unknown.is_empty(),
        "priced but not in the model table: {unknown:?}"
    );
}

#[test]
fn a_real_call_prices_to_a_number_somebody_can_check() {
    // A million input tokens of Sonnet at three dollars, and a million out at fifteen.
    let usage = Usage::absent()
        .with_input(1_000_000)
        .with_cache_read(0)
        .with_cache_write(0)
        .with_output(1_000_000);

    let priced = shipped_prices().price(&ModelId::from("claude-sonnet-5"), &usage);
    assert_eq!(priced.as_ref().map(|p| p.amount), Some(Micros(18_000_000)));
    assert_eq!(
        priced.as_ref().map(|p| p.amount.exact()),
        Some("18.000000".into())
    );
    assert_eq!(
        priced.as_ref().map(|p| p.coverage),
        Some(UsageCoverage::Exact)
    );
    assert_eq!(
        priced.map(|p| p.book),
        Some("anthropic-2026-08".into()),
        "a cost has to name the edition that produced it, or the past gets re-priced by \
         accident later"
    );
}

#[test]
fn a_call_nobody_measured_has_no_price_at_all() {
    // What a subscription command line tool produces on every call. Zero would turn an
    // unknown cost into a free one.
    assert_eq!(
        shipped_prices().price(&ModelId::from("claude-sonnet-5"), &Usage::absent()),
        None
    );
}

#[test]
fn a_price_written_to_seven_places_is_refused() {
    // Seven decimal places is a number copied from a different unit. Rounding it would look
    // correct and be wrong by a factor of ten.
    assert!(Micros::parse("0.1234567").is_err());
    assert_eq!(Micros::parse("0.30"), Ok(Micros(300_000)));
    assert_eq!(Micros::parse("15"), Ok(Micros(15_000_000)));
    assert!(Micros::parse("three dollars").is_err());
}
