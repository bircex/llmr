//! The smallest thing that works.
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo run --example ask -- "what is a monad"
//! ```

use llmr::providers::api::anthropic;
use llmr::{ChatRequest, Message, Provider};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let question = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "Say hello in one sentence.".to_string());

    let claude = anthropic::from_env(Duration::from_secs(60))?;

    let reply = claude
        .chat(
            ChatRequest::new("claude-sonnet-5", vec![Message::user(question)]).with_max_tokens(512),
        )
        .await?;

    println!("{}", reply.text());

    // Worth checking rather than assuming. A reply that hit the output limit arrives with
    // a successful status code and is cut off mid sentence.
    if !reply.is_complete() {
        eprintln!("\n(the answer is incomplete: {:?})", reply.stop_reason);
    }

    Ok(())
}
