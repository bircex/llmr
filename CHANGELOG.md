# Changelog

Versions are calendar versions, `YYYY.M.PATCH`: the year, the month without a leading zero,
and a count of releases in that month from 0, so `2026.9.0`, then `2026.9.1`, then
`2026.10.0`. The number says when, not how much changed; a release that breaks something a
user depends on says so under **Breaking**. Releases before `2026.9.0` used semantic
versioning.

## 2026.9.0 — 2026-09-27

llmr stops being a library and becomes a service: one Docker image, managed over REST, with
its state in an encrypted database. Nothing below existed in 0.2.0, and nothing from 0.2.0
is published as a crate any more.

### Breaking

- Everything a user of 0.2.0 relied on: 0.2.0 was a Rust library, and this is a service. The
  crate is no longer published; see **Removed**. A database from a build of `main` is
  brought forward on start and is not opened by an older build afterwards.

### Added

- Documents and audio in a chat request. A user message may carry a `file` part (a PDF or
  plain text, as a `data:` URL in `file_data`, or bare base64 named `.pdf` or `.txt`) and an
  `input_audio` part (`wav`, `mp3`, `flac`, `ogg`, `aac`, `m4a`, `webm`). `file_id` is refused,
  because llmr has no files endpoint. Two new model capabilities, `documents` and `audio`,
  route them as `images` does: a model without one is skipped. Anthropic takes PDFs and plain
  text and no audio; the OpenAI shape takes PDFs and `wav` or `mp3`; Gemini takes both
  inline. The shipped tables set `documents` on every Anthropic, OpenAI and Gemini row, and
  `audio` on `gemini-3.5-flash-lite` and `gemini-2.5-flash` only, the rows whose model pages
  were read.
- Four endpoints beside chat, in the OpenAI shape: `POST /v1/embeddings` (`float` or
  `base64`), `POST /v1/images/generations`, `POST /v1/audio/speech` (the recording itself,
  typed by `Content-Type`) and `POST /v1/audio/transcriptions` (multipart; `json` or `text`).
  OpenAI and OpenAI-compatible providers serve all four; Gemini serves embeddings, images and
  speech. Fields that cannot be carried are refused by name, and a model that declines is a
  `422 refused`. Each answer carries the `x-llmr-*` headers, a JSON answer carries
  `llmr_cost`, and each is recorded in usage like a chat call.
- A `kind` on every model: `chat`, `embedding`, `image`, `speech` or `transcription`, set with
  `PUT /manage/providers/{id}/models/{model}`. A model of a kind other than chat takes no
  capabilities and needs none to be enabled. `GET /manage/provider-types` lists the `kinds`
  each type offers, model views show `kind`, and the `PUT` answer adds the `endpoint`.
- A route set serves the kind of its first enabled route, shown as `kind`; a route of another
  kind is reported `unavailable`. A set that is not chat tries its routes in order with the
  set's `retry_attempts`, `deadline_secs` and `on_device`, and stops on a refusal; `order` and
  `breaker` apply to chat only.
- `400 wrong_endpoint`: a name sent to the endpoint of another kind is told where it belongs.
  `GET /v1/models` entries carry `llmr_kind`.
- Engine features `image-generation` and `audio`, turned on by `server`, with the modules
  `image`, `audio`, `providers::openai::{image, audio}` and `providers::gemini::media`.
- The service: an `llmr` binary behind the `server` feature, run as the Docker image
  `ghcr.io/<owner>/llmr` (amd64 and arm64).
- The client API, in the OpenAI chat completions shape: `POST /v1/chat/completions`, whole
  and streamed, and `GET /v1/models`. A client asks for a route set such as `default`, or for
  an enabled model as `provider/model`. Fields that cannot be carried to every provider are
  refused by name; a disabled model is refused as `model_not_enabled`; usage nobody reported
  is left out rather than zero. `x-llmr-route`, `x-llmr-attempts` and `x-llmr-fell-through`
  on every reply.
- The management API under `/manage`: provider types, providers (add, change, remove, test
  for free or with one live call), the models each serves (listed by the provider, enabled or
  not, with capabilities for models this release does not know), every enabled model, route
  sets (routes, order, on-device floor, retries, breaker, deadline, and what is usable,
  unavailable or resting), and status. Changes take effect on the next request.
- State in SQLite on the container's volume, with every provider credential sealed with
  XChaCha20-Poly1305 under `LLMR_MASTER_KEY`. The key never touches the volume; a wrong key is
  refused at startup. Credentials are write only over the API.
- `llmr keygen` to make a master key, and `llmr healthcheck` for the image.
- Optional `LLMR_TOKEN`: tokens every call must present. Without it nothing is checked, and
  llmr says so at every start.
- Workflows: the image built on every pull request and started twice (refusing without a
  master key, serving with one), published from `main` and from `v*` tags; a `v*` tag also
  creates a GitHub release with this changelog's section as notes.
- Documentation for running it: `docs/MANAGEMENT.md`, `docs/API.md`, `docs/DEPLOYMENT.md`.
- Usage: every request the client API handles is recorded in the database (what was asked
  for, the route, tokens, cost, latency, outcome; never content), written in the background
  so no request waits on it. `GET /manage/usage` totals any time range, overall or grouped by
  model, provider, name or day, with one amount per currency and a `cost_complete` flag;
  `GET /manage/usage/requests` pages through single requests; `DELETE /manage/usage` forgets
  rows before a time. Each reply carries its own cost as `llmr_cost` (and `x-llmr-cost`):
  `priced`, `partial` (a floor), `unpriced` or `free` for a self hosted model. Schema
  version 2; a version 1 database is brought forward on start.

- Prices kept current without a new image. Three sources, each overruling the one before:
  the shipped tables; a daily sync from a public price list, LiteLLM's by default
  (`LLMR_PRICE_SYNC`, `LLMR_PRICE_SYNC_URL`, `LLMR_PRICE_SYNC_HOURS`); and prices set by hand
  per provider and model (`PUT` and `DELETE /manage/providers/{id}/prices/{model}`), which
  also price `openai-compatible` and self hosted providers. A synced price that moves by
  more than half, or drops to nothing, is held until accepted or rejected
  (`POST /manage/prices/held/{vendor}/{model}`); a refused price is not held again. Context
  banded models stay unpriced. `GET /manage/prices` shows the last sync, held prices and
  prices set by hand; `POST /manage/prices/sync` syncs now; `GET
  /manage/providers/{id}/prices` shows what a provider charges per model and where each
  price came from; `/manage/status` gains `prices`.
- Rates by the picture, the second of audio and the million characters, beside the token
  rates, so image, speech and transcription models are priced: an image endpoint counts the
  pictures it returned, a transcription the seconds the vendor reported, speech the
  characters read aloud. A model sold by a unit nobody measured is `unpriced`, never zero.
  The engine gains `Units`, `PriceBook::price_with`, `PriceBook::new`, and `Rate::tokens`
  with `with_image`, `with_audio_second` and `with_character`.
- Every priced cost names the book edition that priced it: `book` in `llmr_cost` and on a
  usage row's `cost` (`anthropic-2026-09`, `synced-2026-09-27`, `manual-2026-09-27`).
- A weekly workflow, `prices.yml`, compares the shipped price books with the price list and
  keeps one issue open listing the rows that differ, for a person to check against the
  vendor's page. It closes the issue once they agree.

### Changed

- Versions are calendar versions, `YYYY.M.PATCH`: the year, the month without a leading zero,
  and a count of releases that month from 0. This release, the first since 0.2.0, is
  `2026.9.0`; there is no 0.3.0. The image is tagged `2026.9.0` and `2026.9`. The number no
  longer says whether something broke, so a release that does says so under **Breaking**.
- Schema version 4: `models` gains a `kind` column. A version 3 database is brought forward on
  start, and every model in it is a chat model.
- Schema version 5: synced prices, prices set by hand, and `price_book` on usage rows. A
  version 4 database is brought forward on start; its usage rows keep their cost, with no
  book.
- The compatibility promise covers the client API, the management API, the headers, the
  environment variables and the database schema, not the engine's Rust types.
- `Cargo.lock` is committed, because the image has to be reproducible.
- CONTRIBUTING, DESIGN, SECURITY and BEDROCK describe a service rather than a library.
- Four engine error messages had lost the line breaks inside them to long runs of spaces;
  they read as one sentence again.

### Removed

- Publishing to crates.io. `Cargo.toml` says `publish = false`, and the release workflow no
  longer runs `cargo publish`; the `crates-io` environment and `CARGO_REGISTRY_TOKEN` secret
  are no longer read. 0.1.0 stays on crates.io; nothing newer will be published there.
- Rustdoc as a deliverable: the docs.rs configuration, the `docsrs` attributes, the crate
  README, and the `cargo doc` CI steps. Doc comments stay, as comments.
- The pull request jobs that guarded the crate's public API: `cargo package`,
  `cargo publish --dry-run` and `cargo-semver-checks`.
- The engine's `cli` feature: `providers::cli::LocalCli`, the `anthropic::cli` and
  `openai::cli` presets, `Reach::LocalCli` (`local-cli`), `Provider::subscription` and
  `Ledger::record_subscription`. llmr reaches models over their APIs only.
- Command line providers. Builds of `main` before this release briefly ran Claude Code, Codex
  and Gemini CLI inside the image; none of that was released. A database written by such a
  build is brought forward to schema version 3 on start: providers of type `claude-code`,
  `codex` or `gemini-cli` are deleted with their model settings and a warning is logged,
  route sets that named them report those routes as unavailable, and their usage rows stay as
  history.

### Fixed

- The shipped Anthropic prices for `claude-opus-5`, `claude-opus-4-8` and `claude-sonnet-5`
  were the older, higher rates: $15/$75 per million for the two Opus models where the price
  is $5/$25, and $3/$15 for Sonnet 5 where it is $2/$10, with cache reads and writes to
  match. Calls to them were priced one and a half to three times too high. The book is now
  `anthropic-2026-09`; usage rows already recorded keep the cost they were recorded with.

## 0.2.0 — 2026-09-07

### Added

- `Budget`, `Router::within`, `Router::spending` and `Router::charge`, so a run has a cap
  that refuses before the money goes rather than a report afterwards. A route nobody can
  price is refused rather than run blind, a route in another currency is refused rather than
  converted, and what a budget cannot promise is written down: the prompt is not priced
  before it is sent, concurrent calls can overshoot, and a streamed call has to be settled by
  the caller. (#40)
- `Error::OverBudget`, for a call that was never made because it would have taken the run
  over its cap. Nothing was sent, nothing was billed, and it opens no circuit. (#40)
- `Router::within_deadline`, a bound over the whole routed attempt. Checked before every
  attempt and before every retry wait, so a `Retry` policy's attempt count is a maximum
  rather than a promise. It answers `Error::Timeout` rather than the last error from a route,
  because giving up on time and giving up on failures need different fixes. It cannot cut
  short a call already in flight; that is your transport's timeout. (#45)
- `Order` and `Router::ordering`, so selection can be something other than the order you
  wrote: `Cheapest` by published rate, or `Healthiest` by fewest consecutive failures. A
  route with no price sorts **last**, never first, because reading a missing price as zero
  would put every unpriced provider at the front and look deliberate. The requirement check
  still comes first, and every sort is stable. (#44)
- `Route::priced_by` and `Route::rate`, which is where `Order::Cheapest` gets its numbers.
  (#44)
- `Breaker` and `Router::breaking`, so a route that has been failing is skipped for a while
  instead of being tried first, waited on and fallen through in every request. It opens for a
  failure about the provider and never for one about the request, honours a `Retry-After`
  exactly, leaves a rejected credential alone for a long time, and reopens on its own because
  time does it rather than anything having to be called. Off unless you ask, like `Retry`.
  (#42)
- `Router::preflight` now feeds the breaker when there is one: a route that answered
  `Access::Denied` at startup rests for `Breaker::settled` instead of being tried first in
  every request. `Access::Unknown` still does nothing, which is what the third variant is
  for. The route is rested rather than dropped, so a key fixed while the program runs is
  found. (#43)
- `Router::resting`, which routes are currently being skipped and for how long, so the same
  fact is readable without making a request. (#42)
- `Router::stream`, so the crate routes a streamed call and not only a whole one. It falls
  through a provider that fails while the stream is opening and never after the first event
  has reached the caller: continuing a half written answer on a second model produces text
  nobody wrote, in one voice, with nothing downstream able to detect it. (#41)
- `Ledger::record_cancelled`, for a call that went out and whose reply was never read. The
  case is hedging: two providers asked the same question, the loser's future dropped. That
  request was billed, no `ChatResponse` came back, so nothing called `Ledger::record` and the
  ledger reported one measured call and an `Exact` total. (#48)
- `Envelope::with_stop_reason`, so a command line preset can read why the model stopped when
  the tool prints it. A reason this crate has not seen stays `Other` rather than being mapped
  to the nearest one. (#46)
- Shipped model tables and price books for OpenAI and Gemini, beside the Anthropic ones,
  through `openai::api::shipped_registry` / `shipped_prices` and the same pair on `gemini`.
  Both `from_env` constructors now hand out a real table instead of `Registry::empty`. Every
  row was read off a vendor page on the date it carries, and models published in context
  bands are absent rather than priced for short prompts. (#39)
- `PriceBook::age`, `PriceBook::needs_rechecking` and `Recheck`, so a table that has aged can
  be found out about rather than producing a confident bill six months after anybody looked.
  `PriceBook::RECHECK_AFTER_DAYS` is the rule, and it is 90. (#39)
- `PriceBook::expires_on`, for a book that already knows when its numbers stop being right:
  an introductory rate with a published end date, a contract that runs out. The shipped
  Gemini book carries one. (#39)
- `UsageCoverage::Estimated` and `Usage::estimating`, so tokens counted locally can be added
  up without being folded into `Exact`. An estimate is not a floor: it can run high, so
  `Total::About` is a third answer and outranks `Total::AtLeast` when both apply.
  `Ledger::estimated` keeps the guessed part of a bill findable after it has been summed.
  This crate does not count the tokens and will not; bring your own. (#38)
- `Ledger::record_subscription`, `Ledger::subscribed` and `Ledger::plans`, so a call covered
  by a flat fee is out of scope rather than unknown. A run of a hundred command line calls
  reported "at least 0.00" before this, which is true and useless on the crate's main path.
  The total never contains the fee. (#38)
- `Provider::subscription`, defaulting to `None`, and `LocalCli::billed_by` to set it.
  Nothing guesses: the same tool signed in against an API key is metered. (#38)
- `Ledger::record_from`, which reads `Provider::subscription` and records the call the right
  way, so a route added later cannot be recorded two different ways in two places. (#38)
- `Ledger::summary`, the run in one sentence: what was measured, what was estimated, what has
  no figure at all, and what a plan covers. (#38)

### Changed

- **Breaking:** `PriceBook`, `Registry`, `UsageCoverage`, `Total`, `Reach`, `Role`, `Effort`,
  `Thinking` and `Method` are now `#[non_exhaustive]`. Outside code can no longer build them
  with a literal or match them without a `_` arm, which is the point: the next field or
  variant any of them gains is a minor bump rather than a major one. Three of them grew in
  this release and cost one. `Micros` is deliberately left alone, and so is any struct whose
  fields are already private. (#70)

- **Breaking:** `Routed` is now `Routed<T = ChatResponse>`. Written as `Routed` it means what
  it always did; `Router::stream` answers a `Routed<()>` beside the stream. (#41)
- **Breaking:** `Usage` has a new `estimated` field, `Line` a new `subscription` field, and
  `Total` a new `About` variant. A `match` on `Total` needs the arm; both structs are
  `#[non_exhaustive]`, so only a struct literal inside this crate had to change. (#38)
- **Breaking:** `PriceBook` has a new `expires_on` field. A book built with a struct literal
  needs it; one parsed from TOML does not, and a file without the key reads as `None`. (#39)

### Fixed

- The Claude Code preset reported `StopReason::Other` for every reply, on the grounds that a
  command line tool does not say why it stopped. A recorded run shows it does: the envelope
  carries `stop_reason`. `Other` is not `is_complete`, so a caller asking whether an answer
  finished was told "no" for every call it ever made. `Envelope::with_stop_reason` reads it,
  and a tool that says nothing is still `Other`. (#46)

### Testing

- `tests/what_a_command_line_tool_prints.rs`, which drives each command line preset through a
  scripted runner replaying a real recorded envelope, and puts both presets through the
  contract suite for the first time. `tests/recorded/claude-code.json` is the recording:
  `claude 2.1.196`, 2026-08-31. It settles the question a fixture cannot, which is whether
  the tool's `input_tokens` is the whole prompt or the uncached remainder. It is the
  remainder, which is what this crate means, so those names were right. (#46)
- `tests/against_a_real_endpoint.rs`, an opt-in suite that calls each shipped provider for
  real. Every test is `#[ignore]` and a missing key skips rather than fails. It asserts the
  things a fixture cannot: usage that came back `Exact` rather than `Partial`, a reply that
  named a real model, a stop reason that mapped rather than fell back, and a streamed call
  agreeing with a whole one against the wire. `LLMR_RECORD` writes what came back, so a real
  reply can be committed as a fixture. (#37)
- A manually dispatched `Against a real endpoint` workflow, gated behind an environment, so
  the suite is runnable rather than theoretical and never runs on a push. (#37)

### Documentation

- `docs/DESIGN.md` records what a budget checks before a call, what it cannot check, and the
  two races it admits rather than hides. (#40)
- `docs/DESIGN.md` records what a deadline bounds, what it cannot bound, and why the reason
  is the error's type rather than an entry in `fell_through`. (#45)
- `docs/DESIGN.md` records what `Order::Cheapest` is claiming, and why an unpriced route
  sorts last rather than first. (#44)
- `docs/DESIGN.md` records what `preflight` now changes, and why `Unknown` still changes
  nothing. (#43)
- `docs/DESIGN.md` records why the circuit uses atomics and a monotonic clock, which failures
  open it, and why nothing has to reopen one. (#42)
- `docs/DESIGN.md` records why a streamed route stops being replaceable at the first event.
  (#41)
- `docs/DESIGN.md` records that hedging is the caller's to build, what building it here would
  have cost, and the ledger debt it leaves. (#48)
- `docs/DESIGN.md` records why a subscription call is out of scope rather than unknown, why
  an estimate outranks a floor, and why this crate will not ship a tokeniser. (#38)
- `docs/DESIGN.md` records what a shipped table claims, why a banded price is left out
  entirely, and why the tables are not behind a feature. (#39)
- `docs/BEDROCK.md`, the worked example the signing decision needed: what SigV4 covers and in
  what order to attach it, why the transport wrapper is the last thing to touch the request,
  the colon in a Bedrock model id and what it does to a canonical URI, where the region comes
  from, and why rotating credentials belong in the transport. The wrapper is also a compiled
  doctest on `providers::bedrock`, so the half that touches this crate's API cannot rot. (#47)
## 0.1.0 — 2026-08-30

First release.

### Added

- `Provider`, the one trait, with `chat`, `capabilities`, `catalogue` and `validate`.
- `Reach`, separating where a model runs from which vendor made it, with `is_on_device` and
  `uses_local_credential` as two distinct questions.
- `ModelCapabilities` per model and reach, and `ChatRequest::needs` to find out what a
  provider would drop before you send anything.
- `Usage` with `UsageCoverage`, so a call nobody measured reports as absent rather than zero.
- `Access`, with `Ready`, `Denied` and `Unknown`, so a provider that could not be checked is
  told apart from one that refused. `Provider::validate` answers it without sending a
  billable request, and `Router::preflight` asks every route once at startup.
- A model catalogue for the Anthropic provider, which is also what makes its `validate`
  answer more than `Unknown`.
- `LocalCli::with_probe`, so a command line tool says at startup that it is missing or signed
  out rather than inside the first request.
- Providers: Anthropic Messages API, any OpenAI compatible endpoint, and a local command
  line tool run as a subprocess.
- `Registry` and `PriceBook`, both carrying where their facts came from and when a person
  last checked them.
- `testkit`, a contract suite for providers written outside this crate, including
  `assert_a_bad_credential_is_denied`: a rejected key reported as `Unknown` reads as "ask
  again later", so nobody is ever told to fix it.
- A dated Anthropic model table and price book, both refusing a row with no provenance.
- Examples: `ask`, `what_it_cost`, `anything_openai_shaped`, `routing` and `is_it_reachable`.

### Added, since the restructure

- `Provider::stream`, with a default that calls `chat` and hands the finished reply over as
  one burst of events. A provider implementing only `chat` still compiles and still answers.
- `chat::stream`: `Event`, and `Transcript` to fold events back into the `ChatResponse` a
  whole call would have returned. The contract suite checks the two agree.
- `StopReason::Interrupted`, for a stream that stopped arriving before the model was done.
  It is not a reason a provider reports — it is what this crate knows when a stream ends
  without one — and it lives beside the others so `is_complete` already catches it.
- `ModelCapabilities::streaming` and `Requirements::streaming`, so a caller who needs the
  reply word by word can find out by asking instead of by watching a blank screen.
- `HttpTransport::send_streaming`, defaulting to one whole call yielded as a single chunk.
  It checks the status before handing over any bytes, because a 429 has nowhere to go once
  the first chunk has been read as content.
- Server sent event framing in `providers::api`, shared, plus `Protocol::stream_body` and
  `Protocol::read_event`. Both shipped protocols implement them.
- One dependency: `futures-core`, for the `Stream` trait. There is none in std yet, and
  `futures` proper would pull a combinator stack this crate has no use for.

### Added, phase 3

- `retry::Retry`, a policy the caller configures and `Router::retrying` applies. Honours a
  `Retry-After` exactly, jitters what it computes itself, never repeats a rejected
  credential, a malformed request, a refusal or an unreadable reply, and does not repeat a
  timeout unless asked — a second attempt can buy two answers to one question.
- `retry::Delay`, so waiting goes through something you supply. `TokioDelay` behind the
  `retry` feature is the one this crate ships; the trait is always there for another runtime.
- `Routed::attempts`, so a reply that cost three calls says so.
- `tracing` spans behind a feature, carrying provider, model, reach, usage coverage, route
  and attempts — and never the prompt or the credential. With the feature off the crate
  gains no dependency and does no work.
- `UsageCoverage::as_str` and `Display`, one spelling for records and spans.

### Added, phase 4

- `deny.toml` and a CI job: licences against an allowlist rather than a denylist, advisories
  denied with no blanket ignores, sources limited to crates.io, duplicates warned.
- A release workflow on `v*` tags. It re-runs every check against that commit, refuses if the
  tag disagrees with `Cargo.toml` or the changelog has no section for it, and holds the
  publish behind an environment so a person approves it.
- A packaging job on pull requests, so what would ship is checked before release day.
- `cargo-semver-checks`, skipped with a note until 0.1.0 is published and in place for the
  release after it.
- Issue templates, a pull request template carrying the commands every change is held to,
  and a code of conduct.

### Added, images

- `ContentBlock::Image` and `ImageSource`, carrying bytes or a URL with a media type the
  caller gives. Both protocols write it: Anthropic as base64 with the media type beside it,
  the OpenAI shape as a data URL inside a parts array — and a turn with no image keeps the
  plain string content the smaller endpoints speaking that shape require.
- `ModelCapabilities::images`, `Needs::images` and `Requirements::images`, so a reach that
  speaks only text refuses rather than dropping the image. A reply that answered about a
  picture it never received is the failure this prevents.
- `Entry` is `#[non_exhaustive]` with `Entry::new` and builders. Adding `streaming` and then
  `images` broke its struct literal twice, which is exactly what the crate's own rule about
  public structs exists to stop.
- One crate: `base64`, behind the protocol features, because putting image bytes on a wire
  is the one thing a pure translation cannot do with nothing.

### Added, the ledger

- `cost::ledger::Ledger` and `Total`, adding up a run and saying whether the figure is the
  whole of it. One unpriced call makes the total a lower bound; the call is still counted,
  because "forty calls, thirty priced" is not "thirty calls"; and pricing happens once at
  record time, so a newer table cannot rewrite what an older call cost.

### Narrowed

- `Priced`, `ToolSchema`, `Attempted` and `UsageNames` are `#[non_exhaustive]`, with
  constructors for the two callers build. `Priced` was the pointed one — it had no currency
  field, which is fixed below, and `#[non_exhaustive]` is why adding it cost nothing.
- The public API's four external crates are written down in `docs/DESIGN.md` — `serde_json`,
  `serde`, `futures-core` and `reqwest` all appear in signatures callers write, so a major
  bump of any is a breaking change here and nothing in the manifest says so.
- The public item count has a stated method for the first time. It was 180 in the roadmap and
  189 in the issue, and neither said what it counted.

### Added, a cloud partner

- `providers::bedrock::api`, behind a `bedrock` feature: Anthropic's models through Amazon,
  reaching [`Reach::CloudPartner`] for the first time. It reuses the Messages translation
  rather than copying it, with a test asserting both routes send the same request.
- **Signing is the transport's job.** There is no key argument: SigV4 covers the whole HTTP
  request and needs a clock, a region and rotating credentials, none of which a pure
  `Protocol` may hold. This crate ships the translation and you wrap your transport.
- No streaming through Bedrock's binary event framing, so `stream` falls back to one burst —
  an answer rather than a refusal, with `capabilities` saying which it is.

### Fixed, money

- `Priced::currency`, copied from the book that priced the call. `Micros` is an integer and
  two of them add whether or not they are the same money: before this, a ledger holding one
  call priced in dollars and one in euros produced a number that was neither, and looked
  exactly like a number that was.
- `Ledger::total` returns `Option<Total>` and answers `None` for such a run. `Ledger::totals`
  gives one figure per currency, `Ledger::currency` names the single one when there is one,
  and `Ledger::currencies` tells "nothing was priced" apart from "more than one currency".
  An unpriced call makes every currency's figure a floor, not one of them: nothing records
  which currency it would have been billed in.
- No exchange rate, deliberately. A rate has a date and a source exactly like a price does,
  and one invented so that a method could return a single number would produce a figure
  nobody could audit.

### Added, embeddings

- `embed`, behind an `embeddings` feature: `Embedder`, `EmbedRequest`, `Embedding`,
  `Embeddings`, `EmbeddingCapabilities` and `Purpose`. A trait of its own rather than a method
  on `Provider`, and in this crate rather than a second one — #26 decided both, and
  `docs/DESIGN.md` says what a separate crate would have cost.
- **A vector carries the model that made it**, and `Embedding::similarity` answers `None`
  across two of them. The currency rule in a different type: two vectors from two models
  occupy unrelated spaces, and comparing them returns a confident number that means nothing.
- **The reply is index for index with the request.** Vendors send an `index` because their
  arrays carry no order, and a provider that trusts arrival order files every document under
  another document's meaning while nothing fails.
- `testkit::assert_embedder_contract`, which checks that by embedding each input alone and
  asking where it lands. `tests/an_embedder_honours_the_contract.rs` runs it against an
  endpoint double that reverses every reply, and holds a deliberately broken embedder against
  the suite to prove the suite has teeth.
- `providers::openai::embed`, the first implementation: anything speaking `/v1/embeddings`,
  with the reach given rather than guessed. It refuses a batch that comes back short, a row
  with no index, and a dimension count that was asked for and not honoured.
- `Usage::embedding`, so a call that reported everything there was reads as `Exact` rather
  than `Partial`. Without it one embedding anywhere in a run would make every `Ledger` total a
  floor for good.
- No new dependency.

### Added, a second embedder

- `providers::gemini::embed`, behind `gemini` and `embeddings`. It is here for what it
  disagrees with: `taskType` gives `Purpose` somewhere to go, its reply carries no index so
  position is the whole promise, and it reports no usage at all.
- **`Purpose` now reaches a wire.** It shipped in the public API with no implementation
  writing it anywhere, which meant a caller setting it got the same vector either way and
  nothing said so. `EmbeddingCapabilities::purposes` is true for this reach and false for the
  other, which is the question a caller should be asking.
- Both embedders pass `testkit::assert_embedder_contract` unchanged, which is what makes it a
  specification rather than a description of the first one.
- `examples/nearest.rs`: three documents, a question, and a ranking. Both things it prints
  are about refusing — `similarity` returns an `Option` and the ledger says "at least".

### Fixed, checks that could not see

- Two README doctests named feature gated items and so compiled under `--all-features` and
  nowhere else. `cargo test` on the default feature set had been failing for as long as CI
  ran only the all-features line, which it now does not: there is a default-features test
  pass beside it, the same fix the `cargo doc` job needed for the same reason.

### Decided

- **Where a gateway lives** (#29). The top level of `providers::` names who you reach and
  whose credential pays, which for a first party API is the vendor and for a gateway is the
  gateway: `providers::bedrock::api`, not `providers::anthropic::bedrock`. Nothing moved; the
  rule the tree was already following is now stated accurately, and the friendlier option is
  rejected in `docs/DESIGN.md` with what it would cost.
- **Embeddings stay in this crate** (#26), as their own trait behind a feature rather than a
  second crate or a method on `Provider`. What breaks under a separate crate is written down:
  two crates in lockstep, and a `Usage` from one version that is not a `Usage` from the other.

### Changed

- Providers are grouped by vendor and then by reach: `providers::anthropic::{api, cli}` and
  `providers::openai::{api, cli}`, where they were `providers::api::{anthropic, openai}` and
  `providers::cli::{claude, codex}`. Which vendor is what a caller knows first, and the same
  models turn up behind more than one reach — the Messages API and Claude Code were two
  directories apart, and named inconsistently while they were there.
- `providers::api` and `providers::cli` keep the machinery every provider of that kind
  shares, `Protocol` and `ApiProvider` and `LocalCli`. What is shared still follows the
  reach; what is chosen now follows the vendor.
- `providers::api` is no longer behind a feature. `Protocol` is the extension point for a
  protocol nobody has written yet, and it should not need a vendor's feature switched on.

### Not in this release

Reranking, completion endpoints, audio and documents. See the README for what each one costs
you.
