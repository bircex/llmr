//! Ask something, then work out what it cost and how much of that is known.
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo run --example what_it_cost
//! ```

use llmr::providers::anthropic;
use llmr::{ChatRequest, Message, Provider, UsageCoverage};
use std::time::Duration;

/// The day this is being run, as `YYYY-MM-DD`.
///
/// Hard coded because an example should not pull a date library to make one point. A real
/// program has a clock and knows what today is.
const TODAY: &str = "2026-08-31";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let claude = anthropic::api::from_env(Duration::from_secs(60))?;

    let reply = claude
        .chat(
            ChatRequest::new(
                "claude-sonnet-5",
                vec![Message::user("Name three sorting algorithms.")],
            )
            .with_max_tokens(256),
        )
        .await?;

    println!("{}\n", reply.text());
    println!("model reported: {}", reply.model);
    println!("usage:          {:?}", reply.usage.coverage());

    let prices = anthropic::api::shipped_prices();

    // Before anything is presented as a bill. A shipped table is a convenience and not a
    // contract: it was right on the day somebody read the page, and a price that is quietly
    // six months old produces a confident figure that is wrong by whatever the vendor
    // changed, with nothing downstream able to tell.
    //
    // The date is passed in rather than read from a clock, because this crate does not hold
    // one. Whatever your program already uses to know what day it is, use that.
    if let Some(why) = prices.needs_rechecking(TODAY) {
        println!("price table:    {why}\n");
    }

    match prices.price(&reply.model, &reply.usage) {
        Some(priced) => {
            println!("cost:           {} {}", priced.amount, prices.currency);
            println!(
                "priced by:      {} (checked {})",
                priced.book, prices.verified_at
            );

            // The number is only as complete as the usage behind it. Saying so is the
            // difference between a total and a guess that looks like one.
            if priced.coverage != UsageCoverage::Exact {
                println!(
                    "\nthis understates the bill: the provider reported only part of what \
                     the call used"
                );
            }
        }
        // Not zero. Either nothing was measured, or this edition has no rate for the model
        // that actually served the request.
        None => println!("cost:           unknown"),
    }

    Ok(())
}
