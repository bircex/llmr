//! Two providers, one router, and the floor that is not a preference.
//!
//! ```sh
//! cargo run --example routing
//! ```
//!
//! Needs no key and reaches nothing. Both providers are unreachable on purpose, which is
//! what makes the last case worth watching.

use llmr::providers::api::openai;
use llmr::{
    transport::Reqwest, Message, Provider, Reach, Registry, Requirements, Route, Router, Secret,
};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let transport = Arc::new(Reqwest::new(Duration::from_secs(5))?);

    // A model on this machine, and a hosted one. Same protocol, completely different places
    // for your data to go, and only you can say which is which.
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
        Route::new(hosted as Arc<dyn Provider>, "some-hosted-model"),
    ]);

    for (name, capabilities) in router.routes() {
        println!("route {name:<28} {:?}", capabilities.map(|c| c.reach));
    }
    println!("unusable routes: {:?}\n", router.unusable());

    let request = llmr::ChatRequest::new("", vec![Message::user("Summarise this.")]);

    // Ordinary work. The local one is tried first and, being unreachable here, the hosted
    // one answers. `fell_through` is where that shows up.
    println!("--- no privacy floor");
    match router.chat(request.clone(), Requirements::default()).await {
        Ok(routed) => println!(
            "answered by {} after {:?}",
            routed.route, routed.fell_through
        ),
        Err(e) => println!("both were unreachable: {e}"),
    }

    // The same call, with a floor. The hosted provider is not tried at all, however
    // unreachable the local one is. That refusal is the feature.
    println!("\n--- data must stay on this machine");
    match router
        .chat(request, Requirements::default().on_device())
        .await
    {
        Ok(routed) => println!("answered by {}", routed.route),
        Err(e) => println!("refused rather than sent away: {e}"),
    }

    Ok(())
}

/// A one model table, so the router has capabilities to read.
fn shelf(model: &str, reach: Reach) -> Registry {
    let toml = format!(
        "provider = \"example\"\nreach = \"{reach:?}\"\n\n\
         [[model]]\nid = \"{model}\"\ncontext_window = 128000\nmax_output = 4096\n\
         source = \"an example\"\nverified_at = \"2026-08-28\"\n"
    );
    Registry::parse(&toml).unwrap_or_else(|_| Registry::empty("example", reach))
}
