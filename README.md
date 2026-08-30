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

# // The snippet above the block says to turn `reqwest` on, and `from_env` is one of the
# // constructors it provides. Guarded so this compiles under the default feature set too.
# #[cfg(feature = "reqwest")]
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
supply your own transport, which costs 31 crates instead of 105.

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

### Whether you can reach it has three answers

`capabilities()` says what a model can do. It cannot say whether your key works, whether your
account may reach that model, or whether the tool is installed. `validate` asks that, without
sending a request and without spending anything.

```rust,no_run
use llmr::{Access, Provider};

# async fn example(provider: &impl Provider) {
match provider.validate(&"claude-sonnet-5".into()).await {
    // Nothing found that would stop a call.
    Access::Ready => {}
    // Settled. A key, an entitlement, an install: somebody has to fix it.
    Access::Denied { reason } => eprintln!("no, and it will stay no: {reason}"),
    // Nothing established. Still worth trying, and not grounds for dropping the provider.
    other => eprintln!("could not tell: {other}"),
}
# }
```

Three answers rather than two, because `Unknown` is the one a boolean loses. A network that
was down while the check ran is not a provider that refused, and collapsing them takes a
working provider out of a router for a reason that had cleared before anybody read the log.

Call it once at startup over the whole router:

```rust,no_run
# async fn example(router: &llmr::Router) {
for (route, access) in router.preflight().await {
    println!("{route:<28} {access}");
}
# }
```

`preflight` reports and does not prune, and nothing on the request path calls it. Validating
per request would double every round trip to learn something that was almost always true.

`cargo run --example is_it_reachable` prints one line per route.

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

`Router::preflight()` is the other half, and neither one finds what the other does. `unusable`
is about your configuration; `preflight` is about the outside world, and it catches the key
that was rejected and the tool that is not installed.

## Providers

| Module | Feature | Reach | Covers |
|---|---|---|---|
| `providers::anthropic::api` | `anthropic` | first party API | Anthropic Messages |
| `providers::anthropic::cli` | `cli` | local CLI | The Claude Code tool |
| `providers::openai::api` | `openai` | you say | Anything speaking OpenAI chat completions |
| `providers::openai::cli` | `cli` | local CLI | The Codex tool |
| `providers::gemini::api` | `gemini` | first party API | Gemini `generateContent` |
| `providers::bedrock::api` | `bedrock` | cloud partner | Anthropic's models through Amazon |
| `providers::openai::embed` | `openai` + `embeddings` | you say | Anything speaking OpenAI embeddings |
| `providers::gemini::embed` | `gemini` + `embeddings` | first party API | Gemini `batchEmbedContents` |

The last two implement `Embedder` rather than `Provider`. They are siblings of `api` rather
than something inside it, because an embedding call shares a base URL and a key with chat and
nothing else.

The top level names **who you reach and whose credential pays** — the vendor for a first
party API, the gateway for a gateway. `bedrock` is its own node rather than a folder inside
`anthropic` because Claude through Bedrock is not Anthropic answering: a different endpoint,
a different credential, a different company holding your prompt.

They are grouped that way and then by reach, because who you are reaching is what you know first
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
| `anthropic`, `openai` | 31 | Both protocols. You supply the transport |
| `+ reqwest` | 105 | And a bundled client, with `from_env` |
| `cli` alone | 30 | A local tool as a subprocess, no network code |
| `embeddings` | 30 | Text as vectors, its own trait |
| `testkit` | 30 | The contract suite for your own providers |

Counted as distinct crates compiled — `cargo tree` output with duplicate nodes collapsed.
These read 52 and 250 until somebody checked: those were `cargo tree | wc -l`, which counts
a crate once for every dependent that reaches it. The ratio was about right and every figure
was not.

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
# // Compiled only when the feature that provides these modules is on. Without the guard
# // this block is a compile error under the default feature set, which `cargo test
# // --all-features` is the one build that cannot see.
# #[cfg(feature = "cli")]
# fn presets() {
use llmr::providers::{anthropic, openai};
use std::time::Duration;

let tool = anthropic::cli::provider(Duration::from_secs(300)).serving(["claude-sonnet-5"]);
let other = openai::cli::provider(Duration::from_secs(300)).serving(["gpt-5.3-codex"]);
# }
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
OPENAI_API_KEY=... cargo run --example nearest --features openai,embeddings,reqwest
cargo run --example anything_openai_shaped
```

`anything_openai_shaped` needs no key. It sets up three very different endpoints through one
provider and asks each what it serves, which is the quickest way to see what `Reach` is for.

`nearest` embeds three documents and a question and ranks them, and both things it prints are
about refusing: `similarity` returns an `Option`, and the ledger says "at least" because it
ships no price book.

## Streaming

`chat` waits for the whole reply. `stream` hands it over as it arrives.

```rust,no_run
use llmr::chat::stream::Transcript;
# async fn example(p: &impl llmr::Provider, request: llmr::ChatRequest) -> llmr::Result<()> {
let mut transcript = Transcript::new(request.model.clone());
let outcome = transcript.drain(p.stream(request).await?).await;

let reply = transcript.finish();
if let Err(cut_short) = outcome {
    // What arrived is still yours, and the reply says it did not finish.
    eprintln!("stopped early: {cut_short}");
}
# Ok(())
# }
```

Every provider answers `stream`. One that cannot really stream sends the finished reply as a
single burst, which is an answer rather than a refusal — the same text, the same usage, just
all at once. Ask `capabilities()` which pairings do it for real, or set
`Requirements::streaming()` and let the router pick one.

Three things are easy to get wrong here and are handled rather than left to you.

**Usage arrives last, and absent is still not zero.** A stream reports its token counts in
the final frame. One cut off before that reports what really arrived and `None` for the rest,
never a zero standing in for a number nobody sent. The OpenAI shape is asked for usage
explicitly, because it sends none otherwise.

**Reasoning keeps its signature.** A thinking block assembled from deltas ends up with the
provider's signature attached. Without it the *next* turn is rejected, which is a long way
from the mistake.

**A stream that breaks is not a call that failed.** `Transcript::drain` returns the error and
leaves the transcript intact, so what arrived, that the turn did not finish, and why are three
separate answers rather than one guess.

## Trying again

Nothing retries unless you say so. `Error::is_retryable` says a failure was not your fault
and not permanent; whether the *request* is safe to repeat is a question about your request,
and this crate cannot answer it.

```rust,no_run
use llmr::retry::Retry;
use std::time::Duration;

# #[cfg(feature = "retry")]
# fn example(router: llmr::Router) -> llmr::Router {
router.retrying(Retry::new(3).with_base(Duration::from_millis(200)))
# }
```

Four failures are never repeated, because each returns the same answer the second time:
a rejected credential, a malformed request, a refusal and a reply that could not be read.

**A timeout is not repeated either, unless you ask.** The deadline passed; the work may not
have. A second attempt can leave you billed for two answers to one question, so
`repeating_timeouts()` is a decision you make rather than one made for you.

**A wait the provider named is used exactly.** No jitter, no doubling, no ceiling applied to
it. The provider is telling you when the limit clears, and a local timer that fires sooner
turns one rate limit into two. Waits this crate computes for itself are jittered, so two
callers that failed together do not come back together.

`Routed::attempts` says how many calls a reply actually cost, and each retry leaves a line in
`fell_through`. A call paid for twice should not be invisible.

## Spans

Behind the `tracing` feature, off by default. With it off the crate gains no dependency and
does no work on the path a request takes; a library that emitted whether you asked or not is
one people work around.

A span carries which provider, which model, which reach, how complete the usage was, which
route answered, and how many attempts it took. **Never the prompt and never a credential** —
that is the shape of the code rather than a promise, and there is a test that says so.

The line worth having is a warning on a *successful* call that did not take the first route.
Nothing failed, and something is going wrong.

## Images

`ContentBlock::Image` carries bytes or a URL, with the media type you give it — sniffing it
here would be this crate deciding something you already know, and a provider told the wrong
type either rejects the request or decodes it wrongly.

A reach that speaks only text cannot carry one at all, and that is the point: it is a
capability, so `needs().unmet_by()` names it before anything is sent and a provider that
cannot carry it **refuses rather than dropping it**. A reply that answered confidently about
a picture it never received is the failure this prevents, and nothing in that reply says so.

## Embeddings

Behind the `embeddings` feature, and a trait of their own rather than a method on `Provider`.
An embedding call is a different request, a different reply and a different usage shape, and
adding `embed` to `Provider` would make every chat-only provider implement a refusal.

```rust,no_run
# #[cfg(all(feature = "openai", feature = "embeddings", feature = "reqwest"))]
# async fn example() -> llmr::Result<()> {
use llmr::embed::{EmbedRequest, Embedder};
use llmr::providers::openai;
use std::time::Duration;

let embedder = openai::embed::from_env(Duration::from_secs(30))?;

let stored = embedder
    .embed(EmbedRequest::new(
        "text-embedding-3-small".into(),
        vec!["the tide came in".into(), "compile times are long".into()],
    ))
    .await?;

let asked = embedder
    .embed(EmbedRequest::one("text-embedding-3-small", "when is high water"))
    .await?;

// `None` would mean these vectors are not comparable. They are, so this is a number.
let nearest = stored.get(0).and_then(|v| asked.get(0)?.similarity(v));
println!("{nearest:?}");
# Ok(())
# }
```

**A vector belongs to the model that made it.** Two vectors of the same length from two
different models are not comparable, and nothing about them says so: cosine similarity
computes happily and returns a confident number that means nothing. It is the same failure as
adding dollars to euros, so it is caught the same way — every `Embedding` carries its model
and `similarity` returns `None` rather than a number across two of them.

**The reply is index for index with the request.** Several vendors send an `index` on every
row precisely because their arrays are not ordered, and a provider that trusts arrival order
pairs every document with another document's vector. Nothing downstream fails: the index
builds, the queries run, and the results are wrong. `testkit::assert_embedder_contract` sends
a batch through a deliberately shuffled endpoint and checks each vector came back where it
was put.

**A dimension count asked for and not honoured is refused.** A caller who asked for 256 has
sized something for 256, and an endpoint that ignores the parameter answers full length
vectors with a 200.

Two implementations ship, and the second is there for what it disagrees with. Gemini's shape
has a `taskType`, so `Purpose` reaches a wire and `capabilities(...).purposes` is true; its
reply carries no index, so position is the whole promise and a count that does not match is
unrecoverable rather than merely wrong; and it reports no usage at all, so every call is
`absent` and a ledger holding one says "at least". Both pass the same contract suite. A suite
one implementation passes is a description of that implementation.

`examples/nearest.rs` embeds three documents and a question and ranks them.

## Model tables and prices

`Registry` holds what a model can do. `PriceBook` holds what it costs. Both carry where the
facts came from and when a person last checked them, because a table with no date on it is a
set of claims and there is no way to find out which of them went stale.

Neither is required. A provider with an empty registry answers `None` for every model, which
reads as "this provider does not know", and that is the honest answer when nobody has written
the table down.

Historical costs should never be recomputed. `Priced` records which book edition produced it,
so re-pricing the past is a choice rather than an accident.

`Ledger` adds up a run. The arithmetic is the easy half:

```rust
use llmr::cost::ledger::Ledger;
# use llmr::{ChatResponse, PriceBook};
# fn example(api: &ChatResponse, through_a_tool: &ChatResponse, book: &PriceBook) {
let mut ledger = Ledger::new();
ledger.record(api, Some(book));
ledger.record(through_a_tool, None);   // a subscription tool has no price row

assert_eq!(ledger.calls(), 2);
assert_eq!(ledger.unpriced(), 1);

// `None` would mean the run mixes currencies. It does not, so there is a figure.
let total = ledger.total().expect("one book, so one currency");
assert!(!total.is_exact());             // but the figure is a floor, and says so
# }
```

**One unpriced call makes the whole total a lower bound**, and `Total` is two variants rather
than a number and a flag, because a flag is something a caller can forget to read. The
unpriced call is still counted: "forty calls, thirty of them priced" is a different sentence
from "thirty calls". And pricing happens once, at the moment of recording, so a table updated
next week cannot rewrite what a call cost last week.

**A sum needs one currency.** `Micros` is an integer and two of them add whether or not they
are the same money, so `Priced` carries the code its book was written in, `total` answers
`None` when a run mixes them, and `totals` gives one figure per currency instead. There is no
exchange rate in here: a rate has a date and a source exactly like a price does, and one
invented to make a method return a number would produce a figure nobody could audit.

## What this crate does not do

It does not decide what your work needs. `Router` picks a provider that meets a set of
requirements; deciding that a code review needs reasoning and a commit message does not is
policy over your own system, and it belongs where your roles and your rules are.

It is not an agent framework. No tool loop, no memory, no orchestration. It answers one
question: how do I reach this model, and what did it cost.

## What is not here yet

Said plainly, because finding a gap by hitting it is worse than reading about it.

**Audio and documents.** `ContentBlock` carries text, reasoning, tool calls and images.
Nothing else yet.

**Reranking and completion endpoints.** Embeddings are here, behind the `embeddings`
feature. Nothing else that is not chat.

**Model catalogues, on the command line providers.** All three API providers implement
`catalogue()`. A command line tool cannot be asked what it serves, so it answers
`Error::Unsupported`, which is an answer and not an empty list.

**A free way to check a command line login.** `validate` on a CLI provider probes the program
and establishes that it is installed. No vendor tool answers "is this login still good"
without doing work, so a `Ready` from that reach is a weaker claim than one from an API
provider, and it says so.

## Contributing

[CONTRIBUTING.md](CONTRIBUTING.md) has the rules the code is held to and how to add a
provider. [docs/DESIGN.md](docs/DESIGN.md) says what was decided and why, which is worth
reading before changing anything: several of the decisions look wrong until you know the
reason. [ROADMAP.md](ROADMAP.md) is what is left before 0.1.

## License

MIT. See [LICENSE](https://github.com/recepkizilarslan/llmr/blob/main/LICENSE).
