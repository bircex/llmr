//! Ask something, then work out what it cost and how much of that is known.
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo run --example what_it_cost
//! ```

use llmr::providers::anthropic;
use llmr::{ChatRequest, Message, Provider, UsageCoverage};
use std::time::Duration;

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
