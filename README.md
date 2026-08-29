# llmr

Reach language models across providers, with capabilities you can read before you ask and
usage you can trust.

```toml
[dependencies]
llmr = { version = "0.1", features = ["reqwest"] }
```

```rust,no_run
use llmr::providers::anthropic;
use llmr::{ChatRequest, Message, Provider};
use std::time::Duration;

# async fn example() -> llmr::Result<()> {
let claude = anthropic::api::from_env(Duration::from_secs(60))?;

let reply = claude
    .chat(ChatRequest::new("claude-sonnet-5", vec![Message::user("Hello")]))
    .await?;

println!("{}", reply.text());
# Ok(())
# }
```

The `reqwest` feature is the bundled HTTP client. Without it you get both protocols and
supply your own transport, which costs 52 crates instead of 250.

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
use llmr::Reach;

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
use llmr::{ChatRequest, Message, ModelCapabilities, Reach};

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
use llmr::{Usage, UsageCoverage};

assert_eq!(Usage::absent().coverage(), UsageCoverage::Absent);
```

A cost built from partial usage says so too, so a total can be honest about what it does not
know.

## Routing

A bag of providers is not a layer. `Router` is what makes it one: describe what a request
needs, and it picks something that can serve it, in the order you chose, falling through when
one is unreachable.

```rust,no_run
use llmr::{Requirements, Route, Router};
# use std::sync::Arc;
# async fn example(local: Arc<dyn llmr::Provider>, hosted: Arc<dyn llmr::Provider>) -> llmr::Result<()> {
let router = Router::new(vec![
    Route::new(local, "llama3"),
    Route::new(hosted, "claude-sonnet-5"),
]);

let request = llmr::ChatRequest::new("", vec![llmr::Message::user("Summarise this.")]);

// Read what the request needs, then add what only your program knows.
let routed = router
    .chat(request.clone(), Requirements::of(&request).on_device())
    .await?;

println!("{} answered", routed.route);
# Ok(())
# }
```

It routes on three things and no others: what the request needs, where the data may go, and
the order you gave. There is nothing in here about a task being a security review or a
summary, because that is a fact about your system rather than about a model. A router that
knew what a security review was would be one only its author could use.

Two behaviours are worth knowing before you rely on it.

**A privacy floor is a floor, not a preference.** `Requirements::on_device()` means a hosted
provider is never tried, even when every local one is down. A fallback that ignored it would
send a customer record to a vendor the first time something was slow, and every log line
about it would say the call succeeded.

**A refusal stops.** When a model declines, the next one is not asked the same question.
That is shopping a policy decision around until something agrees, and it is what you get by
accident if refusals are treated like any other error.

`Routed::fell_through` lists what was tried first and why each one did not answer. It is
empty on the common path, and a non empty list on a *successful* call is a provider degrading
while nothing is failing.

`Router::unusable()` reports routes no request can ever select, which is almost always a typo
in a model name. Worth calling at startup, because otherwise it looks like a fallback that is
configured and simply never needed.

## Providers

| Module | Feature | Reach | Covers |
|---|---|---|---|
| `providers::anthropic::api` | `anthropic` | first party API | Anthropic Messages |
| `providers::anthropic::cli` | `cli` | local CLI | The Claude Code tool |
| `providers::openai::api` | `openai` | you say | Anything speaking OpenAI chat completions |
| `providers::openai::cli` | `cli` | local CLI | The Codex tool |

They are grouped by vendor and then by reach, because which vendor is what you know first
and the same models turn up behind more than one reach. Anthropic's are reachable over the
API and through Claude Code, and those two differ in what they can carry rather than in what
they are, so the choice belongs in one place.

Grouping this way does not soften what `Reach` is for. It is still the axis that decides
where your data goes and whose credential pays, and it still travels on `capabilities()`
where a caller can read it before sending. A module path could never be read that way:
`anthropic::api` and `anthropic::cli` are the same vendor and the same models, and they are
not the same place for a prompt to go.

The OpenAI provider is written against a shape rather than a vendor. OpenAI, Groq, Together,
Fireworks, vLLM, Ollama, LM Studio, OpenRouter and LiteLLM all answer at
`/v1/chat/completions` with the same envelope, so the base URL is a constructor argument and
one provider covers them all. It sits under `openai` because that is what the ecosystem
calls the shape, not because reaching Ollama is reaching OpenAI.

Which is why it is the one provider whose reach you supply. Everywhere else the module
settles it; there a model on your laptop and a hosted API look identical from here and are
not the same place for your data to go.

## Features

| Feature | Crates | What you get |
|---|---:|---|
| `anthropic`, `openai` | 52 | Both protocols. You supply the transport |
| `+ reqwest` | 250 | And a bundled client, with `from_env` |
| `cli` alone | 53 | A local tool as a subprocess, no network code |
| `testkit` | 52 | The contract suite for your own providers |

The first two are on by default. `reqwest` is not, because almost every program already has
an HTTP client and adding this crate should not add two hundred more.

```toml
llmr = { version = "0.1", default-features = false, features = ["cli"] }
```

## Adding a provider

The transport, the credential, the status codes and the error mapping are shared. A provider
supplies only the part that differs.

Note where the two live. What is *shared* is under the reach, because reach is what decides
how a model is spoken to. What is *chosen* is under the vendor, because that is what a caller
picks. So you build on `providers::api` and `providers::cli`, and what you write lands beside
`providers::anthropic` and `providers::openai`.

For something over the network, implement `providers::api::Protocol`: what URL, what headers,
what JSON goes out, what comes back. There is no client in it and nowhere to hold state, so a
protocol is a set of pure functions and the shared machinery does the rest.

For a command line tool, there is not even that. `providers::cli::LocalCli` does the spawning,
the deadline, the kill on drop and the difference between a missing binary and a silent one.
A vendor preset is a program name, its arguments, and the shape of what it prints, which is
why the vendor files are forty lines:

```rust,no_run
use llmr::providers::{anthropic, openai};
use std::time::Duration;

let tool = anthropic::cli::provider(Duration::from_secs(300)).serving(["claude-sonnet-5"]);
let other = openai::cli::provider(Duration::from_secs(300)).serving(["gpt-5.3-codex"]);
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
llmr = { version = "0.1", features = ["testkit"] }
```

```rust,no_run
# #[cfg(feature = "testkit")]
# async fn example(mine: &impl llmr::Provider) {
use llmr::testkit::assert_provider_contract;

assert_provider_contract(mine, "the-model-you-serve").await;
# }
```

Every provider in this crate passes the same suite. A suite only one implementation can pass
has stopped being a specification.

## Examples

```sh
ANTHROPIC_API_KEY=... cargo run --example ask -- "what is a monad"
ANTHROPIC_API_KEY=... cargo run --example what_it_cost
cargo run --example anything_openai_shaped
```

The last one needs no key. It sets up three very different endpoints through one provider
and asks each what it serves, which is the quickest way to see what `Reach` is for.

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

It does not decide what your work needs. `Router` picks a provider that meets a set of
requirements; deciding that a code review needs reasoning and a commit message does not is
policy over your own system, and it belongs where your roles and your rules are.

It is not an agent framework. No tool loop, no memory, no orchestration. It answers one
question: how do I reach this model, and what did it cost.

## What is not here yet

Said plainly, because finding a gap by hitting it is worse than reading about it.

**Streaming.** `chat` sends a request and waits for the whole reply. There is no token by
token API. If you are building something a person watches, this will feel slow, and no
workaround here will fix it. It changes the shape of the `Provider` trait, so it is a version
rather than a patch.

**Retries.** `Error::is_retryable` tells you a failure was not your fault and not permanent.
Nothing acts on it. That is deliberate for now: a retry is safe or not depending on your
request, and a library that decided for you would double a bill on a timeout.

**Images and other input.** `ContentBlock` carries text, reasoning and tool calls. No images,
audio or documents.

**Anything that is not chat.** No embeddings, no reranking, no completion endpoints.

**Model catalogues, mostly.** Only the OpenAI shaped provider implements `catalogue()`. The
others answer `Error::Unsupported`, which is an answer and not an empty list.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) has the rules the code is held to and how to add a
provider. [docs/DESIGN.md](docs/DESIGN.md) says what was decided and why, which is worth
reading before changing anything: several of the decisions look wrong until you know the
reason. [ROADMAP.md](ROADMAP.md) is what is left before 0.1.

## License

MIT. See [LICENSE](https://github.com/recepkizilarslan/llmr/blob/main/LICENSE).
