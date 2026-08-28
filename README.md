# modelreach

Reach language models across providers, with capabilities you can read before you ask and
usage you can trust.

```toml
[dependencies]
modelreach = "0.1"
```

```rust,no_run
use modelreach::providers::anthropic::Anthropic;
use modelreach::{ChatRequest, Message, Provider};
use std::time::Duration;

# async fn example() -> modelreach::Result<()> {
let claude = Anthropic::from_env(Duration::from_secs(60))?;

let reply = claude
    .chat(ChatRequest::new("claude-sonnet-5", vec![Message::user("Hello")]))
    .await?;

println!("{}", reply.text());
# Ok(())
# }
```

## What this is for

Most libraries that put many providers behind one interface do it by finding the subset they
all share. That subset does not include prompt caching, reasoning blocks, or the difference
between a prompt token and a cached one, and those are where the money and the quality are.

This crate keeps the rich shape and asks each provider to say what it could not carry.

Three ideas hold it together.

### Reach is not the same as provider

Where a model runs decides where your data goes and whose credential pays. It is a separate
axis from which vendor made the model, and collapsing the two is how a customer record ends
up at a third party.

```rust
use modelreach::Reach;

// A vendor command line tool signs in on your laptop and still sends every prompt away.
assert!(Reach::LocalCli.uses_local_credential());
assert!(!Reach::LocalCli.is_on_device());

// Only one reach keeps the data.
assert!(Reach::SelfHosted.is_on_device());
```

Code that treats "the key is local" as "the data is local" will log a prompt as private and
be wrong.

### Capabilities belong to the pair, not the model

The same model behind a command line tool usually cannot take a tool schema or return a
cache breakpoint. That is a fact about the reach, not about the model, so `capabilities()`
answers for the pairing.

Ask before you send, and find out what would be dropped:

```rust
use modelreach::{ChatRequest, Message, ModelCapabilities, Reach};

let request = ChatRequest::new("some-model", vec![Message::user("hi")])
    .with_response_schema(serde_json::json!({ "type": "object" }));

let through_a_cli = ModelCapabilities::none(Reach::LocalCli);
assert_eq!(request.needs().unmet_by(&through_a_cli), vec!["structured_output"]);
```

Without that, you find out by reading a reply that quietly ignored half of what you asked
for, and paying for it.

### Usage that was never reported is absent, not zero

A subscription command line tool reports no token counts. Writing zero would turn an unknown
cost into a free one in every report that adds it up.

```rust
use modelreach::{Usage, UsageCoverage};

assert_eq!(Usage::absent().coverage(), UsageCoverage::Absent);
```

A cost built from partial usage says so too, so a total can be honest about what it does not
know.

## Providers

| Provider | Feature | Reach | Covers |
|---|---|---|---|
| `anthropic` | `anthropic` | first party API | Anthropic Messages API |
| `openai` | `openai` | you say | Any endpoint speaking OpenAI chat completions |
| `cli` | `cli` | local CLI | A vendor command line tool as a subprocess |

The OpenAI provider is written against a shape rather than a vendor. OpenAI, Groq, Together,
Fireworks, vLLM, Ollama, LM Studio, OpenRouter and LiteLLM all answer at
`/v1/chat/completions` with the same envelope, so the base URL is a constructor argument and
one provider covers them all.

The reach is given rather than guessed, because a model on your laptop and a hosted API look
identical from here and are not the same place for your data to go.

Each provider is behind a feature, so a program that only reaches a local tool does not build
an HTTP client and a TLS stack.

```toml
modelreach = { version = "0.1", default-features = false, features = ["cli"] }
```

## Concurrency

Every provider is immutable once built. There is no lock and no interior mutability, so one
instance serves any number of concurrent calls with nothing to contend on.

That is enforced rather than intended. `await_holding_lock` and `await_holding_refcell_ref`
are denied across the crate, so the way an async program deadlocks cannot be introduced
without the build failing. `unsafe_code` is forbidden.

If you write a provider of your own, keep to the same rule: `chat` takes `&self`, so anything
you share must be immutable after construction or behind an atomic.

## Writing your own provider

Implement `Provider`, then check it against the contract suite:

```toml
[dev-dependencies]
modelreach = { version = "0.1", features = ["testkit"] }
```

```rust,no_run
# #[cfg(feature = "testkit")]
# async fn example(mine: &impl modelreach::Provider) {
use modelreach::testkit::assert_provider_contract;

assert_provider_contract(mine, "the-model-you-serve").await;
# }
```

Every provider in this crate passes the same suite. A suite only one implementation can pass
has stopped being a specification.

## Model tables and prices

`Registry` holds what a model can do. `PriceBook` holds what it costs. Both carry where the
facts came from and when a person last checked them, because a table with no date on it is a
set of claims and there is no way to find out which of them went stale.

Neither is required. A provider with an empty registry answers `None` for every model, which
reads as "this provider does not know", and that is the honest answer when nobody has written
the table down.

Historical costs should never be recomputed. `Priced` records which book edition produced it,
so re-pricing the past is a choice rather than an accident.

## What this crate does not do

It does not choose a model for you. Picking one for a task is policy over your own fleet, and
that belongs where your roles and your rules are.

It is not an agent framework. No tool loop, no memory, no orchestration. It answers one
question: how do I reach this model, and what did it cost.

## License

MIT. See [LICENSE](https://github.com/birceX/modelreach/blob/main/LICENSE).
