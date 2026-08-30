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
- `cargo-semver-checks`, which has nothing to compare against until 0.1.0 and is in place for
  the release after it.
- Issue templates, a pull request template carrying the seven commands, and a code of
  conduct.

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
