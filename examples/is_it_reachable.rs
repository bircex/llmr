//! Which of your providers actually work, asked once, before anything is sent.
//!
//! ```sh
//! cargo run --example is_it_reachable
//! ```
//!
//! Needs no key. Both endpoints here are meant to be unreachable, which is the point: what
//! you are watching is the difference between a settled no and a question that could not be
//! answered.

use llmr::providers::api::openai;
use llmr::{transport::Reqwest, Access, Provider, Reach, Registry, Route, Router, Secret};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let transport = Arc::new(Reqwest::new(Duration::from_secs(5))?);

    // A model on this machine, and a hosted one. Neither is running, so the answers below
    // come from the two ways that can happen.
    let local = Arc::new(openai::at(
        "ollama",
        "http://localhost:11434/v1",
        Arc::clone(&transport) as Arc<dyn llmr::transport::HttpTransport>,
        Secret::new("ollama", "not-needed"),
        Reach::SelfHosted,
        Arc::new(shelf("llama3", Reach::SelfHosted)),
    ));

    let hosted = Arc::new(openai::at(
        "vendor",
        "https://api.example.invalid/v1",
        transport,
        Secret::new("vendor", "sk-not-real"),
        Reach::FirstPartyApi,
        Arc::new(shelf("some-hosted-model", Reach::FirstPartyApi)),
    ));

    let router = Router::new(vec![
        Route::new(local as Arc<dyn Provider>, "llama3"),
        // A typo, left in deliberately. It is the failure the other check finds.
        Route::new(hosted as Arc<dyn Provider>, "some-hosted-modle"),
    ]);

    // Two questions, both worth asking at startup and neither one finding the other.
    //
    // `unusable` is about your configuration: a route whose provider does not know its
    // model can never be selected, and it is almost always a typo.
    println!("routes nothing can select: {:?}\n", router.unusable());

    // `preflight` is about the outside world: a key that was rejected, a tool that is not
    // installed, an account that cannot reach the model.
    for (route, access) in router.preflight().await {
        let note = match &access {
            // Nothing found that would stop a call. How much that establishes depends on
            // the reach, and it is never a guarantee.
            Access::Ready => "reachable".to_string(),
            // Settled. Somebody has to fix something.
            Access::Denied { reason } => format!("no, and it will stay no: {reason}"),
            // Nothing was established. Still worth trying, and not grounds for dropping it.
            Access::Unknown { why } => format!("could not tell: {why}"),
            // `Access` is non exhaustive, so an answer added in a later version arrives here
            // instead of stopping this from compiling. Read as unknown, which is the only
            // safe reading of an answer this code has never seen.
            other => format!("could not tell: {other}"),
        };
        println!("{route:<28} {:<8} {note}", access.as_str());
    }

    println!(
        "\nNothing above sent a prompt or spent anything. Denied is the one to act on: \
         unknown means ask again."
    );

    Ok(())
}

/// A one model table, so the routes have capabilities to read.
fn shelf(model: &str, reach: Reach) -> Registry {
    let toml = format!(
        "provider = \"example\"\nreach = \"{reach:?}\"\n\n\
         [[model]]\nid = \"{model}\"\ncontext_window = 128000\nmax_output = 4096\n\
         source = \"an example\"\nverified_at = \"2026-08-30\"\n"
    );
    Registry::parse(&toml).unwrap_or_else(|_| Registry::empty("example", reach))
}
