//! The tables that ship with this crate, read the way a caller reads them.
//!
//! They are data in a file, so they can be wrong in a commit nobody looked at closely. A
//! missing date, a model priced twice, a row with no source: every one of those parses fine
//! as TOML and is refused here.
//!
//! Three providers ship tables now rather than one, and every claim below is made of all of
//! them by the same code. A rule that held for Anthropic and was never applied to the next
//! table is a rule that lasted one provider.

// Nothing to check when no provider is compiled in.
#![cfg(any(feature = "anthropic", feature = "openai", feature = "gemini"))]

use llmr::cost::pricing::Recheck;
use llmr::{ModelId, PriceBook, Reach, Registry, Usage};

/// Every shipped pair, so a claim made once is made of all of them.
fn shipped() -> Vec<(&'static str, Registry, PriceBook)> {
    #[allow(unused_mut)]
    let mut all: Vec<(&'static str, Registry, PriceBook)> = Vec::new();

    #[cfg(feature = "anthropic")]
    {
        use llmr::providers::anthropic::api::{shipped_prices, shipped_registry};
        all.push(("anthropic", shipped_registry(), shipped_prices()));
    }
    #[cfg(feature = "openai")]
    {
        use llmr::providers::openai::api::{shipped_prices, shipped_registry};
        all.push(("openai", shipped_registry(), shipped_prices()));
    }
    #[cfg(feature = "gemini")]
    {
        use llmr::providers::gemini::api::{shipped_prices, shipped_registry};
        all.push(("gemini", shipped_registry(), shipped_prices()));
    }

    all
}

#[test]
fn every_shipped_registry_reads_and_is_not_empty() {
    for (name, registry, _) in shipped() {
        assert!(
            !registry.ids().is_empty(),
            "{name}: the table failed to parse"
        );
        assert_eq!(registry.reach, Reach::FirstPartyApi, "{name}");
        assert_eq!(
            registry.provider, name,
            "{name}: the table names another vendor"
        );
    }
}

#[test]
fn every_shipped_price_book_carries_its_provenance() {
    for (name, _, prices) in shipped() {
        assert!(
            !prices.rates.is_empty(),
            "{name}: the price file failed to parse"
        );
        assert!(
            !prices.verified_at.is_empty(),
            "{name}: a price with no date"
        );
        assert!(!prices.source.is_empty(), "{name}: a price with no source");
        assert_eq!(prices.currency, "USD", "{name}");
        assert_eq!(
            prices.provider, name,
            "{name}: the book names another vendor"
        );
    }
}

#[test]
fn every_priced_model_is_one_the_registry_knows() {
    // A price for a model nobody can reach is a row that will never be used, and a sign
    // that one of the two files was edited without the other.
    for (name, registry, prices) in shipped() {
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
            "{name}: priced but not in the model table: {unknown:?}"
        );
    }
}

#[test]
fn every_shipped_row_carries_a_date_this_crate_can_read() {
    // The one thing that would let a table never age. A `verified_at` of "recently" parses
    // as TOML, refuses nothing, and makes `needs_rechecking` unable to say anything, which
    // is worse than a date that has passed because nothing ever reports it.
    for (name, _, prices) in shipped() {
        assert!(
            prices.age("2026-08-31").is_some(),
            "{name}: verified_at is not a date this crate can read"
        );
        assert!(
            !matches!(
                prices.needs_rechecking("2026-08-31"),
                Some(Recheck::Undatable { .. })
            ),
            "{name}: a date on this book cannot be read"
        );
    }
}

#[test]
fn a_shipped_book_is_not_already_stale_on_the_day_it_shipped() {
    // Not a claim about today. It is a claim that the table was current when it was written,
    // which is the only thing a repository can check about itself. What a caller does about
    // the table having aged since is `needs_rechecking`, and that is their call to make.
    for (name, _, prices) in shipped() {
        assert_eq!(
            prices.needs_rechecking(&prices.verified_at),
            None,
            "{name}: shipped stale"
        );
    }
}

#[test]
fn a_call_nobody_measured_has_no_price_in_any_shipped_book() {
    // What a subscription command line tool produces on every call. Zero would turn an
    // unknown cost into a free one.
    for (name, registry, prices) in shipped() {
        for model in registry.ids() {
            assert_eq!(
                prices.price(&model, &Usage::absent()),
                None,
                "{name}: {model} priced a call nobody measured"
            );
        }
    }
}

#[cfg(feature = "anthropic")]
#[test]
fn the_anthropic_registry_knows_the_model_this_crate_names() {
    let registry = llmr::providers::anthropic::api::shipped_registry();
    let caps = registry.capabilities(&ModelId::from("claude-sonnet-5"));
    assert!(
        caps.is_some(),
        "a model this crate names is not in its table"
    );
    assert!(caps.map(|c| c.context_window > 0).unwrap_or(false));
}

#[cfg(feature = "anthropic")]
#[test]
fn a_real_call_prices_to_a_number_somebody_can_check() {
    use llmr::{Micros, UsageCoverage};

    // A million input tokens of Sonnet at three dollars, and a million out at fifteen.
    let usage = Usage::absent()
        .with_input(1_000_000)
        .with_cache_read(0)
        .with_cache_write(0)
        .with_output(1_000_000);

    let priced = llmr::providers::anthropic::api::shipped_prices()
        .price(&ModelId::from("claude-sonnet-5"), &usage);
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

#[cfg(feature = "openai")]
#[test]
fn an_openai_call_prices_to_a_number_somebody_can_check() {
    // A million in at $1.25 and a million out at $10.00, which is what the pricing page says
    // gpt-5 costs. The point of the figure is that a person can check it against the page.
    let usage = Usage::absent()
        .with_input(1_000_000)
        .with_cache_read(0)
        .with_cache_write(0)
        .with_output(1_000_000);

    let priced =
        llmr::providers::openai::api::shipped_prices().price(&ModelId::from("gpt-5"), &usage);
    assert_eq!(
        priced.as_ref().map(|p| p.amount.exact()),
        Some("11.250000".into())
    );
    assert_eq!(priced.map(|p| p.book), Some("openai-2026-08".into()));
}

#[cfg(feature = "openai")]
#[test]
fn a_model_priced_in_context_bands_is_absent_rather_than_priced_for_short_prompts() {
    // gpt-5.5 is published at one rate under 272K tokens and a higher one above. A Rate is
    // a flat number and cannot say that, so the row is not there. Unpriced is the honest
    // answer; the tempting one is the low band, which is right until somebody sends a long
    // prompt and then quietly understates every call after that.
    let prices = llmr::providers::openai::api::shipped_prices();
    assert_eq!(prices.rate(&ModelId::from("gpt-5.5")), None);
    assert_eq!(prices.rate(&ModelId::from("gpt-5.5-pro")), None);
}

#[cfg(feature = "gemini")]
#[test]
fn the_gemini_book_says_when_it_stops_being_right() {
    // One of its rows is an introductory rate with a published end date. Past that date
    // every figure this book produces is wrong by whatever the vendor already announced, so
    // the book carries the date and says so rather than ageing quietly like any other.
    let prices = llmr::providers::gemini::api::shipped_prices();
    assert_eq!(
        prices.expires_on.as_deref(),
        Some("2026-12-31"),
        "the announced end of the introductory rate"
    );

    // Inside both windows: recently checked, and before the increase.
    assert_eq!(prices.needs_rechecking("2026-10-01"), None);

    // Past the increase, this book is wrong by an amount somebody already published, and it
    // is also long past its recheck age. Expiry is the reason reported, because it is the
    // settled one: ageing says "somebody should look", expiry says "these numbers changed".
    assert!(
        matches!(
            prices.needs_rechecking("2027-01-01"),
            Some(Recheck::Expired { days_ago: 1, .. })
        ),
        "the day after an announced increase, this book has to say so: {:?}",
        prices.needs_rechecking("2027-01-01")
    );
}
