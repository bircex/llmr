//! Embed a few documents and a question, and find which document answers it.
//!
//! ```sh
//! OPENAI_API_KEY=... cargo run --example nearest --features openai,embeddings,reqwest
//! ```
//!
//! The two things worth watching for are both about refusing rather than answering.
//!
//! `similarity` returns an `Option`. `None` means the two vectors cannot be compared, which
//! for two models is the case that would otherwise produce a confident number meaning
//! nothing. Here they are from one model, so it is always `Some` — and the type still makes
//! you say what happens if it is not.
//!
//! The ledger says "at least" rather than a figure. An embedding call through this reach
//! reports its prompt tokens and this example ships no price book, so the cost is a call
//! that happened and a number nobody knows. That is what an unpriced call looks like, and it
//! is counted rather than dropped.

use llmr::embed::{EmbedRequest, Embedder, Purpose};
use llmr::providers::openai;
use llmr::Ledger;
use std::time::Duration;

const MODEL: &str = "text-embedding-3-small";

#[tokio::main]
async fn main() -> llmr::Result<()> {
    let embedder = openai::embed::from_env(Duration::from_secs(30))?;

    let documents = vec![
        "The harbour gates close an hour before low water.".to_string(),
        "Monomorphisation is what makes generic code expensive to compile.".to_string(),
        "Poach the eggs for four minutes and no longer.".to_string(),
    ];

    // `Purpose` is sent when the reach has somewhere to put it. This one does not — read
    // `capabilities(...).purposes` to find out, rather than assuming either way.
    let stored = embedder
        .embed(EmbedRequest::new(MODEL.into(), documents.clone()).for_purpose(Purpose::Document))
        .await?;

    let question = "why is my rust build slow";
    let asked = embedder
        .embed(EmbedRequest::one(MODEL, question).for_purpose(Purpose::Query))
        .await?;

    let query = match asked.get(0) {
        Some(vector) => vector,
        None => return Err(llmr::Error::Unreadable("no vector came back".into())),
    };

    println!("{question:?}\n");
    println!(
        "served by {}, {:?} dimensions\n",
        stored.model,
        stored.dimensions()
    );

    let mut scored: Vec<(f32, &String)> = Vec::new();
    for (position, document) in documents.iter().enumerate() {
        // `None` here would mean the two are not comparable. They came from one call to one
        // model, so this is the branch that never runs — and writing it is the point.
        match stored.get(position).and_then(|v| query.similarity(v)) {
            Some(score) => scored.push((score, document)),
            None => println!("  (not comparable: {document})"),
        }
    }
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));

    for (score, document) in &scored {
        println!("  {score:.3}  {document}");
    }

    // Both calls, counted. No price book here, so both are unpriced — which makes the total
    // a floor rather than a figure, and says so.
    let mut ledger = Ledger::new();
    ledger.record_unpriced(stored.model.clone(), stored.usage);
    ledger.record_unpriced(asked.model.clone(), asked.usage);

    let total = match ledger.total() {
        Some(total) => total,
        None => return Err(llmr::Error::Unreadable("more than one currency".into())),
    };
    println!(
        "\n{} calls, {} of them unpriced. Cost: {total}",
        ledger.calls(),
        ledger.unpriced()
    );

    Ok(())
}
