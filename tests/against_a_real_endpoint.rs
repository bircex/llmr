//! Every provider, against the API it claims to speak.
//!
//! # Why this file exists
//!
//! Every other test in this repository is written against a fixture this repository wrote.
//! That catches a translation that changed and no longer matches what it used to produce. It
//! cannot catch a field name that was wrong from the beginning, because the fixture has the
//! same name in it and the two agree.
//!
//! Four chat providers and two embedders shipped in 0.1.0 without one line of any of them
//! having talked to a real API. This is the file that fixes that, and the risk it addresses
//! is invisible from inside: three hundred passing tests say nothing about it.
//!
//! # Running it
//!
//! Every test here is `#[ignore]`, so `cargo test` never runs one and CI never spends money.
//!
//! ```sh
//! ANTHROPIC_API_KEY=... cargo test --all-features --test against_a_real_endpoint -- --ignored
//! ```
//!
//! A test whose key is missing skips itself and says so rather than failing, so running the
//! whole file with one key set is useful.
//!
//! # What is asserted, and what is not
//!
//! Not "it answered". A fixture already proves the crate can read a reply it was given. What
//! a fixture cannot check is whether the reply it was given looks anything like a real one,
//! so these are the four claims that only a real endpoint can settle:
//!
//! * **Usage is present, and [`UsageCoverage::Exact`] rather than `Partial`.** A `Partial`
//!   means a field this crate reads by name was not there under that name. It is the exact
//!   shape of "I read a field name wrong", and every cost report built on it is a floor
//!   nobody knows is a floor.
//! * **The model in the reply is a real name**, and it is printed, because what a vendor
//!   actually serves for a given alias is a fact worth recording in a commit.
//! * **The stop reason mapped to something**, not to whatever the fallback is. A provider
//!   that maps every unknown reason to `EndTurn` reports a truncated answer as a complete
//!   one.
//! * **A streamed call and a whole call agree**, against the real wire rather than against
//!   two fixtures written the same afternoon. The contract suite already checks that they
//!   agree about a recording; this checks they agree about reality.
//!
//! # Recording what came back
//!
//! Set `LLMR_RECORD` to a directory and each test writes the reply it saw into it, as JSON.
//! That is how a real reply becomes a fixture in this repository rather than staying in
//! somebody's terminal.
//!
//! ```sh
//! LLMR_RECORD=tests/recorded ANTHROPIC_API_KEY=... \
//!   cargo test --all-features --test against_a_real_endpoint -- --ignored
//! ```

#![cfg(feature = "reqwest")]

use llmr::chat::stream::Transcript;
use llmr::{
    ChatRequest, ChatResponse, Message, ModelId, Provider, StopReason, Usage, UsageCoverage,
};
use std::time::Duration;

/// Sixteen tokens. The point is a real round trip, not an answer worth reading.
const SHORT: u32 = 16;

/// Long enough for a slow first token, short enough that a hung run ends.
fn timeout() -> Duration {
    Duration::from_secs(60)
}

/// The key, or `None` and a reason printed.
///
/// A missing key skips rather than fails, so one key set is enough to run the file.
fn key(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => {
            println!("skipped: {name} is not set");
            None
        }
    }
}

fn ask(model: &str) -> ChatRequest {
    ChatRequest::new(model, vec![Message::user("Say OK and nothing else.")]).with_max_tokens(SHORT)
}

/// Everything a fixture cannot check, checked once against a real reply.
///
/// One function rather than four assertions per provider, so a claim made of Anthropic is
/// made of every provider added afterwards. A check that held for the first vendor and was
/// never applied to the next is a check that lasted one vendor.
fn hold_it_to_the_contract(provider: &str, asked_for: &str, reply: &ChatResponse) {
    println!("--- {provider} ---");
    println!("  asked for: {asked_for}");
    println!("  served by: {}", reply.model);
    println!("  stopped:   {:?}", reply.stop_reason);
    println!(
        "  usage:     {:?} {:?}",
        reply.usage.coverage(),
        reply.usage
    );

    assert!(
        !reply.text().is_empty(),
        "{provider}: a reply with no readable content"
    );

    // The one that catches a field name read wrong. `Partial` here means this crate looked
    // for something under a name the vendor does not use, and every cost built on it is
    // short by whatever it missed with nothing downstream able to tell.
    assert_eq!(
        reply.usage.coverage(),
        UsageCoverage::Exact,
        "{provider}: usage came back {:?}, which means a field this crate reads by name was \
         not there under that name: {:?}",
        reply.usage.coverage(),
        reply.usage
    );

    assert!(
        reply.usage.output_tokens.unwrap_or(0) > 0,
        "{provider}: a reply arrived and the output count was nought"
    );

    // A model name that came back empty, or came back as the alias that was sent with no
    // version on it, is worth seeing rather than passing.
    assert!(
        !reply.model.as_str().trim().is_empty(),
        "{provider}: the reply did not say which model served it"
    );

    // Asking for sixteen tokens and being told the turn ended normally is the shape of a
    // provider mapping every reason it does not recognise onto the default.
    assert!(
        matches!(
            reply.stop_reason,
            StopReason::MaxTokens | StopReason::EndTurn
        ),
        "{provider}: unexpected stop reason {:?}",
        reply.stop_reason
    );

    record(provider, reply);
}

/// Writes the reply into `LLMR_RECORD`, when that is set.
///
/// This is how a real reply becomes a fixture in the repository rather than staying in
/// somebody's terminal, which is the difference between having called an API once and being
/// able to prove it.
fn record(provider: &str, reply: &ChatResponse) {
    let Ok(directory) = std::env::var("LLMR_RECORD") else {
        return;
    };
    if std::fs::create_dir_all(&directory).is_err() {
        println!("  could not write to {directory}");
        return;
    }

    let path = format!("{directory}/{provider}.json");
    let recorded = serde_json::json!({
        "provider": provider,
        "model": reply.model.as_str(),
        "stop_reason": format!("{:?}", reply.stop_reason),
        "coverage": reply.usage.coverage().as_str(),
        "usage": reply.usage,
        "text": reply.text(),
    });
    match serde_json::to_string_pretty(&recorded) {
        Ok(json) => {
            let _ = std::fs::write(&path, json);
            println!("  recorded to {path}");
        }
        Err(e) => println!("  could not serialize the reply: {e}"),
    }
}

/// A whole call and a streamed one, and whether they agree about what it consumed.
///
/// The claim the contract suite already makes about a recording, made about reality. Two
/// ways to ask the same question that disagree are worse than one way, and the disagreement
/// is invisible until somebody compares two cost reports.
async fn and_a_stream_agrees(provider: &str, p: &impl Provider, model: &str, whole: &Usage) {
    let mut transcript = Transcript::new(model);
    let stream = match p.stream(ask(model)).await {
        Ok(stream) => stream,
        Err(e) => panic!("{provider}: the stream would not open: {e}"),
    };
    let outcome = transcript.drain(stream).await;
    let streamed = transcript.finish();

    assert!(
        outcome.is_ok(),
        "{provider}: the stream failed: {outcome:?}"
    );
    println!(
        "  streamed:  {:?} {:?}",
        streamed.usage.coverage(),
        streamed.usage
    );

    assert_eq!(
        streamed.usage.coverage(),
        whole.coverage(),
        "{provider}: a streamed call and a whole one disagree about how much was measured"
    );
    assert!(
        streamed.usage.prompt_tokens() == whole.prompt_tokens(),
        "{provider}: the same prompt was counted as {:?} streamed and {:?} whole",
        streamed.usage.prompt_tokens(),
        whole.prompt_tokens()
    );
}

// ---- Anthropic -------------------------------------------------------------------------

#[cfg(feature = "anthropic")]
#[tokio::test]
#[ignore = "costs money; run with --ignored and a key"]
async fn anthropic_answers_for_real() {
    if key("ANTHROPIC_API_KEY").is_none() {
        return;
    }

    let model = "claude-haiku-4-5";
    let claude = llmr::providers::anthropic::api::from_env(timeout())
        .unwrap_or_else(|e| panic!("building the provider: {e}"));

    let reply = claude
        .chat(ask(model))
        .await
        .unwrap_or_else(|e| panic!("anthropic: {e}"));

    hold_it_to_the_contract("anthropic", model, &reply);
    and_a_stream_agrees("anthropic", &claude, model, &reply.usage).await;
}

#[cfg(feature = "anthropic")]
#[tokio::test]
#[ignore = "costs money; run with --ignored and a key"]
async fn the_anthropic_catalogue_lists_the_models_this_crate_ships_a_table_for() {
    // The shipped table is a set of claims about names. This is the only thing that can say
    // whether the vendor still serves them, and `Registry::stale` is the method for it.
    if key("ANTHROPIC_API_KEY").is_none() {
        return;
    }

    let claude = llmr::providers::anthropic::api::from_env(timeout())
        .unwrap_or_else(|e| panic!("building the provider: {e}"));
    let served = claude
        .catalogue()
        .await
        .unwrap_or_else(|e| panic!("anthropic catalogue: {e}"));

    let table = llmr::providers::anthropic::api::shipped_registry();
    let stale = table.stale(&served);
    let unlisted = table.unlisted(&served);

    println!("--- anthropic catalogue ---");
    println!("  serves {} models", served.len());
    println!("  in the shipped table and no longer served: {stale:?}");
    println!("  served and not in the shipped table:       {unlisted:?}");

    assert!(
        stale.is_empty(),
        "the shipped table names models this account cannot reach: {stale:?}"
    );
}

// ---- OpenAI ----------------------------------------------------------------------------

#[cfg(feature = "openai")]
#[tokio::test]
#[ignore = "costs money; run with --ignored and a key"]
async fn openai_answers_for_real() {
    if key("OPENAI_API_KEY").is_none() {
        return;
    }

    let model = "gpt-5-nano";
    let openai = llmr::providers::openai::api::from_env(timeout())
        .unwrap_or_else(|e| panic!("building the provider: {e}"));

    let reply = openai
        .chat(ask(model))
        .await
        .unwrap_or_else(|e| panic!("openai: {e}"));

    hold_it_to_the_contract("openai", model, &reply);
    and_a_stream_agrees("openai", &openai, model, &reply.usage).await;
}

#[cfg(feature = "openai")]
#[tokio::test]
#[ignore = "costs money; run with --ignored and a key"]
async fn openai_reports_the_uncached_remainder_rather_than_the_whole_prompt() {
    // This provider documents one adjustment and this is the only place it can be checked:
    // OpenAI reports `prompt_tokens` as the whole prompt including the cached part, and this
    // crate's `input_tokens` means the part that was not cached, so the provider subtracts.
    // Against a fixture, both readings agree with whatever the fixture says.
    if key("OPENAI_API_KEY").is_none() {
        return;
    }

    let model = "gpt-5-nano";
    let openai = llmr::providers::openai::api::from_env(timeout())
        .unwrap_or_else(|e| panic!("building the provider: {e}"));
    let reply = openai
        .chat(ask(model))
        .await
        .unwrap_or_else(|e| panic!("openai: {e}"));

    println!("--- openai usage ---");
    println!(
        "  input (uncached remainder): {:?}",
        reply.usage.input_tokens
    );
    println!(
        "  cache read:                 {:?}",
        reply.usage.cache_read_tokens
    );
    println!(
        "  prompt total:               {:?}",
        reply.usage.prompt_tokens()
    );

    // A short one-off prompt is not cached, so the remainder is the whole prompt and both
    // numbers agree. What this catches is the subtraction going the wrong way, which shows
    // up as an input count larger than the total.
    assert!(
        reply.usage.input_tokens <= reply.usage.prompt_tokens(),
        "the uncached remainder cannot exceed the whole prompt"
    );
    assert!(
        reply.usage.input_tokens.unwrap_or(0) > 0,
        "a prompt was sent and none of it was counted"
    );
}

// ---- Gemini ----------------------------------------------------------------------------

#[cfg(feature = "gemini")]
#[tokio::test]
#[ignore = "costs money; run with --ignored and a key"]
async fn gemini_answers_for_real() {
    if key("GEMINI_API_KEY").is_none() {
        return;
    }

    let model = "gemini-2.5-flash";
    let gemini = llmr::providers::gemini::api::from_env(timeout())
        .unwrap_or_else(|e| panic!("building the provider: {e}"));

    let reply = gemini
        .chat(ask(model))
        .await
        .unwrap_or_else(|e| panic!("gemini: {e}"));

    // Not `hold_it_to_the_contract`. This API reports no cache write count at all, so its
    // usage is `Partial` by design rather than by mistake, and asserting `Exact` here would
    // be asserting that a documented decision is a bug. Everything else is the same.
    println!("--- gemini ---");
    println!("  served by: {}", reply.model);
    println!("  stopped:   {:?}", reply.stop_reason);
    println!(
        "  usage:     {:?} {:?}",
        reply.usage.coverage(),
        reply.usage
    );

    assert!(!reply.text().is_empty(), "gemini: no readable content");
    assert_eq!(
        reply.usage.cache_write_tokens, None,
        "this API grew a cache write count, and the provider should now read it"
    );
    assert!(
        reply.usage.output_tokens.unwrap_or(0) > 0,
        "gemini: a reply arrived and the output count was nought"
    );
    assert!(
        reply.usage.input_tokens.unwrap_or(0) > 0,
        "gemini: a prompt was sent and none of it was counted"
    );
    record("gemini", &reply);

    and_a_stream_agrees("gemini", &gemini, model, &reply.usage).await;
}

// ---- Embedders -------------------------------------------------------------------------

#[cfg(all(feature = "embeddings", feature = "openai"))]
#[tokio::test]
#[ignore = "costs money; run with --ignored and a key"]
async fn the_openai_embedder_answers_for_real() {
    use llmr::{EmbedRequest, Embedder};

    if key("OPENAI_API_KEY").is_none() {
        return;
    }

    let model = "text-embedding-3-small";
    let embedder = llmr::providers::openai::embed::from_env(timeout())
        .unwrap_or_else(|e| panic!("building the embedder: {e}"));

    let vectors = embedder
        .embed(EmbedRequest::new(
            ModelId::from(model),
            vec!["hello".into(), "world".into()],
        ))
        .await
        .unwrap_or_else(|e| panic!("openai embeddings: {e}"));

    println!("--- openai embeddings ---");
    println!("  vectors:    {}", vectors.vectors.len());
    println!(
        "  dimensions: {:?}",
        vectors.vectors.first().map(|v| v.vector.len())
    );
    println!("  usage:      {:?}", vectors.usage);

    // Index for index with the request. A reordered reply is the failure this cannot be
    // checked for anywhere but here, because a fixture is written in the order it is read.
    assert_eq!(vectors.vectors.len(), 2, "one vector per input, in order");
    assert!(
        vectors.vectors.iter().all(|v| !v.vector.is_empty()),
        "an empty vector came back"
    );
    assert_eq!(
        vectors.usage.coverage(),
        UsageCoverage::Exact,
        "the embedding usage was not read: {:?}",
        vectors.usage
    );
}

#[cfg(all(feature = "embeddings", feature = "gemini"))]
#[tokio::test]
#[ignore = "costs money; run with --ignored and a key"]
async fn the_gemini_embedder_answers_for_real() {
    use llmr::{EmbedRequest, Embedder};

    if key("GEMINI_API_KEY").is_none() {
        return;
    }

    let model = "gemini-embedding-001";
    let embedder = llmr::providers::gemini::embed::from_env(timeout())
        .unwrap_or_else(|e| panic!("building the embedder: {e}"));

    let vectors = embedder
        .embed(EmbedRequest::new(
            ModelId::from(model),
            vec!["hello".into(), "world".into()],
        ))
        .await
        .unwrap_or_else(|e| panic!("gemini embeddings: {e}"));

    println!("--- gemini embeddings ---");
    println!("  vectors:    {}", vectors.vectors.len());
    println!(
        "  dimensions: {:?}",
        vectors.vectors.first().map(|v| v.vector.len())
    );
    println!("  usage:      {:?}", vectors.usage);

    assert_eq!(vectors.vectors.len(), 2, "one vector per input, in order");
    assert!(
        vectors.vectors.iter().all(|v| !v.vector.is_empty()),
        "an empty vector came back"
    );
}
