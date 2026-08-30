# Changelog

This project follows [semantic versioning](https://semver.org). Before 1.0, a breaking
change is a minor bump.

## Unreleased

First release. Nothing published yet.

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
- Issue templates, a pull request template carrying the seven commands, and a code of
  conduct.

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

Images, embeddings. See the README for what each one costs you.
