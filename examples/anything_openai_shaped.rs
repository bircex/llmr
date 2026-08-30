//! One provider, three very different endpoints.
//!
//! A model on your laptop and a hosted API answer the same request shape. What separates
//! them is where your data goes, and this crate cannot work that out for you. You say it.
//!
//! ```sh
//! cargo run --example anything_openai_shaped
//! ```

use llmr::providers::openai;
use llmr::{transport::Reqwest, Provider, Reach, Registry, Secret};
use std::sync::Arc;
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let transport = Arc::new(Reqwest::new(Duration::from_secs(120))?);

    let endpoints = [
        (
            "ollama",
            "http://localhost:11434/v1",
            // Nothing leaves this machine.
            Reach::SelfHosted,
            Secret::new("ollama", "not-needed"),
        ),
        (
            "groq",
            "https://api.groq.com/openai/v1",
            Reach::CloudPartner,
            Secret::from_env("groq", "GROQ_API_KEY").unwrap_or_else(|_| Secret::new("groq", "")),
        ),
        (
            "openrouter",
            "https://openrouter.ai/api/v1",
            Reach::CloudPartner,
            Secret::from_env("openrouter", "OPENROUTER_API_KEY")
                .unwrap_or_else(|_| Secret::new("openrouter", "")),
        ),
    ];

    for (id, url, reach, key) in endpoints {
        let provider = openai::api::at(
            id,
            url,
            Arc::clone(&transport) as Arc<dyn llmr::transport::HttpTransport>,
            key,
            reach,
            Arc::new(Registry::empty(id, reach)),
        );

        print!("{:<12} {:<16}", provider.id(), reach.to_string());
        if reach.is_on_device() {
            print!("  data stays here");
        } else if reach.uses_local_credential() {
            print!("  local key, data leaves");
        } else {
            print!("  data leaves");
        }

        // Asking the endpoint what it serves, rather than trusting a table.
        match provider.catalogue().await {
            Ok(models) => println!("  {} models", models.len()),
            Err(e) => println!("  unreachable: {e}"),
        }
    }

    Ok(())
}
