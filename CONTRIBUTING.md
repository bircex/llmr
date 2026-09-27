# Contributing

Thanks for looking. llmr is an LLM router that runs as a Docker container and is managed over
REST: an OpenAI-compatible client API and a management API (`src/bin/llmr/`), over a routing
engine (the rest of `src/`), with its state in an encrypted SQLite database. It has a narrow
job, and the rules below exist to keep it that way.

The Docker image is the only thing it ships. Nothing is published as a crate, so the engine's
Rust types are internal and can change whenever the service needs them to.

## Read this first

[docs/DESIGN.md](docs/DESIGN.md) is the reasoning behind the decisions in this project. A good
number of them look like something to tidy away until you know what breaks without them, and
the tidying is the failure mode this project is most exposed to.

[ROADMAP.md](ROADMAP.md) is what shipped and what is next.

## What belongs here

One question: how do I reach this model, and what did it cost.

Adding a provider, fixing a translation, correcting a model table or a price, carrying a
field the gateway currently refuses: yes.

A tool loop, memory, orchestration, or choosing a model for a task: no. Those are decisions
about your system, and a router that made them would be one you had to fight.

## Before you open a pull request

```sh
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings
cargo test --all-features
cargo test
```

All six must be clean, and warnings count. The repeats are not redundant: a clippy lint can
fire under one feature set and not another, and a doctest naming a feature gated item
compiles under `--all-features` and nowhere else.

Run them on the toolchain in `rust-toolchain.toml` rather than whatever your machine has.
That file exists because these commands once passed on a laptop running 1.97 and failed on
a runner running 1.98 for weeks, with the crate unchanged. `rustup` picks it up on its own
if you are in the repository.

## The rules the code is held to

**No panicking.** `unwrap`, `expect`, `panic!`, `todo!` and `unimplemented!` are denied, in
the engine and in the gateway binary alike. A server that dies on one odd reply takes every
request in flight down with it. Tests may panic; that is their job.

**No lock held across an await.** `await_holding_lock` and `await_holding_refcell_ref` are
denied. A provider holds nothing mutable, so a call needs no lock at all. If you find
yourself wanting one, that is worth discussing in an issue first.

**No unsafe.** Forbidden, not denied.

**Every public item in the engine is documented.** `missing_docs` is denied. Say what it is
for, not what it is. "Returns the model id" is not documentation.

**Nothing the gateway logs can hold a prompt, a reply or a key.** Log the name asked for, the
route, counts and the stop reason. A request body in a log line is a leak in every
deployment that upgrades.

## Where things live

```
src/
  bin/llmr/      the gateway
    main.rs        command line, startup, shutdown, logging, `check` and `healthcheck`
    records.rs     what is stored: provider types, providers, models, route sets
    store.rs       the SQLite database, and every query
    crypto.rs      credentials at rest, sealed with the master key
    gateway.rs     providers and routers built from the store, swapped in on a change
    manage.rs      the management API
    openai.rs      the OpenAI request and reply shape, both directions
    server.rs      the client API, the token check, streaming
    media.rs       the endpoints beside chat: embeddings, images, speech, transcription
    error.rs       failures as OpenAI error bodies and status codes
  chat/          the engine: what a call is made of: message, request, response, stream
  cost/          what it consumed and what that is worth: usage, pricing, ledger
  embed/         text as vectors: the Embedder trait (feature `embeddings`)
  image/         pictures from a prompt: the ImageGenerator trait (feature `image-generation`)
  audio/         speech both ways: SpeechSynthesizer and Transcriber (feature `audio`)
  providers/
    api/         the shared machinery for reaching over the network: ApiProvider + Protocol
    anthropic/   api.rs the Messages protocol
    openai/      api.rs the chat completions shape, embed.rs, image.rs, audio.rs
    gemini/      api.rs generateContent, embed.rs, media.rs (pictures and speech)
    bedrock/     api.rs InvokeModel, reusing the Messages translation
  model.rs       Reach, ModelId, ModelCapabilities
  registry.rs    what a provider serves and what it can do
  provider.rs    the one trait
  router.rs      which provider a request goes to; breaker.rs, retry.rs, budget.rs beside it
  transport.rs   the HTTP boundary, and a reqwest implementation of it
  error.rs secret.rs observe.rs testkit.rs
models/          the shipped model tables and price books, dated
docs/            MANAGEMENT, API, DEPLOYMENT for people running it; DESIGN, BEDROCK for people
                 changing it
Dockerfile, docker-compose.yml, .env.example
.github/workflows/
  ci.yml         every pull request: the six checks, each feature alone, MSRV, cargo deny
  docker.yml     the image: built on pull requests, published from main and from tags
  release.yml    a v* tag: checks, and the GitHub release
  live.yml       calls real providers, by hand only
```

Two groupings in the engine, doing two jobs. **What is shared follows the reach**, because
reach is what decides how a model is spoken to: everything an API provider does apart from
writing JSON is identical. **What is chosen follows the vendor**, because that is what a caller picks, and the same
models turn up behind more than one reach.

So `anthropic/api.rs` is short. The machinery is not in it.

Everything else stays flat. A directory holding one file is a directory that exists to look
organised.

## Changing the gateway

Most changes a user would notice land in `src/bin/llmr/`. Three habits keep it honest:

- **A field is carried or refused, never dropped.** If the OpenAI shape has a field the
  engine cannot carry, `openai.rs` answers `400` naming it. Carrying one means adding it to
  `ChatRequest` and to every protocol, not only to the parser.
- **A management API field is a promise.** A panel is built against it. Add fields with a
  default; renaming or removing one breaks every panel that sends it, so it needs a changelog
  line. Bodies use `deny_unknown_fields`, so a misspelt field is a `400` rather than ignored;
  keep it that way.
- **Secrets go in sealed and come out as a hint.** A credential is sealed in `store.rs`
  before it is written, opened only to build a provider, and shown as its last four
  characters. Nothing returns it, and nothing logs it.
- **A schema change is a migration.** `store.rs` records a schema version; raise it and add
  the step that brings an older database forward. Never edit a released schema in place:
  somebody's volume has it.
- **Test through HTTP.** `server.rs` and `manage.rs` send real requests to the application,
  and `manage.rs` stands up a real OpenAI-compatible endpoint to point providers at. A
  behaviour a client or a panel can see gets a test there, not only a unit test of the
  function underneath.

To see a change for real, build the image and run it:

```sh
docker build -t llmr:dev .
docker run --rm -p 8080:8080 -v llmr-dev:/var/lib/llmr \
  -e LLMR_MASTER_KEY="$(docker run --rm llmr:dev keygen)" llmr:dev
```

## What CI will run

The six above, plus two you would not usually run by hand:

- **`cargo deny check`** — licences against an allowlist, advisories denied, sources limited
  to crates.io, duplicate versions warned. `deny.toml` says why each allowed licence is
  there. Install it with `cargo install cargo-deny --locked` if you want to run it locally.
- **The Docker image** — built on every pull request, then started twice: without a master
  key, which it must refuse, and with one, when it must open a fresh database and answer its
  healthcheck. `docker build .` reproduces it locally.

## Writing a provider

You are almost certainly writing a **protocol**, not a client.

Implement `providers::api::Protocol`: what URL, what headers, what JSON goes out, what comes
back. The transport, the credential, the status codes and the error mapping are
`ApiProvider`'s. There is nowhere to hold state, and that is on purpose.

It goes in a file under **whoever you reach and whoever the credential pays** —
`providers::<who>::api` — beside whatever other reaches that node already has. For a first party API that is the vendor. For a gateway serving several vendors'
models over one credential, such as Bedrock, it is the gateway: `providers::bedrock::api`,
not `providers::anthropic::bedrock`. Claude through Bedrock is not Anthropic answering, and
the import line should not suggest it is.

A new node is a new directory with a `mod.rs` saying which reaches it has and what each one
can carry, since a caller comparing two of them is the reason the directory exists. If it is
a gateway, add a line to each vendor it serves pointing at it — somebody looking for Claude
on Bedrock will look under `anthropic` first, and that pointer is where they are already
looking.

`docs/DESIGN.md` has the reasoning, including the option that was rejected and why.

Then run the contract suite against it:

```rust,no_run
# #[cfg(feature = "testkit")]
# async fn example(mine: &impl llmr::Provider) {
llmr::testkit::assert_provider_contract(mine, "a-model-you-serve").await;
# }
```

Three things the suite is checking, and they are the ones that are easy to get wrong:

1. `capabilities` returns `None` for a model you do not know, and a capability set with
   everything off for a model you know and cannot do much with. Those are different answers
   to different questions.
2. A reply you could not read is an error. Never an empty message. A caller cannot tell an
   empty answer from a failure, and one of them means carry on.
3. Usage the provider did not report is `Usage::absent()`, not zeros. An unknown cost
   written as zero becomes a free call in every report that adds it up.

Put your provider behind a feature, then make it something a panel can add: a
`ProviderType` in `src/bin/llmr/records.rs` (with the `TypeInfo` a panel renders a form
from, and the `kinds` it has endpoints for), a branch in `build_provider` in `gateway.rs`
that builds the chat provider and any of the embedder, image, speech and transcription
endpoints those kinds need, and a row in `docs/MANAGEMENT.md`. A
provider the management API cannot add is a provider nobody running llmr can use.

### And then call it for real, once

The contract suite and every fixture in this repository are written here, which means they
catch a translation that changed and cannot catch a field name that was wrong from the
beginning: the fixture has the same wrong name in it and the two agree.

`tests/against_a_real_endpoint.rs` is the file that settles that. Add your provider to it,
and run it once with a key:

```sh
YOUR_API_KEY=... cargo test --all-features --test against_a_real_endpoint -- --ignored --nocapture
```

Every test in it is `#[ignore]`, so nobody runs one by accident and CI never spends money.
A test whose key is missing skips itself and says so.

What it asserts is deliberately not "it answered", because a fixture already proves that.
It asserts the things a fixture cannot: that usage came back `Exact` rather than `Partial`,
which is the exact shape of a field name read wrong; that the reply named a real model; that
the stop reason mapped to something rather than to a fallback; and that a streamed call and a
whole call agree about what was consumed, against the wire rather than against two fixtures
written the same afternoon.

Set `LLMR_RECORD` to a directory and it writes what came back, so a real reply becomes a
fixture in this repository rather than staying in your terminal. Commit that with the
provider. The `Against a real endpoint` workflow does the same thing on a runner and is
dispatched by hand, never on a push.

## Model tables and prices

Every row carries a `source` and a `verified_at`. A row without them is refused at parse
time, and that is deliberate: the first time one number turns out to be wrong, there is no
way to tell which of the others still hold.

If you update a table, update the date, and say in the pull request where you checked.

## Tests

Name a test after the claim it makes, not after the function it calls.
`a_reply_with_no_readable_content_is_an_error_not_an_empty_answer` tells a reader what
breaks if it fails. `test_chat` does not.

If a test needs a comment to explain why the claim matters, write the comment. The next
person to see it will be deciding whether to delete it.

## Commits and versions

Versions follow semantic versioning, applied to what users of llmr depend on: the client
API, the management API, the response headers, the environment variables, and the database
on their volume. Before 1.0, a breaking change to any of those is a minor bump and gets a
line in `CHANGELOG.md`. A database that a new release cannot open is the worst kind of
break, which is why schema changes are migrations.

The engine's Rust types are not part of that promise. Most of them are `#[non_exhaustive]`
and built through constructors, which keeps changes to them local; if you add a struct the
gateway must build, give it a constructor in the same commit.

A release is a tag, `vX.Y.Z`, matching the version in `Cargo.toml` and a section in
`CHANGELOG.md`. The tag publishes the Docker image and the GitHub release; nothing is
published from a laptop.
