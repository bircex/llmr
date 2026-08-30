# Changelog

This project follows [semantic versioning](https://semver.org). Before 1.0, a breaking
change is a minor bump.

## Unreleased

First release. Nothing published yet.

### Added

- `Provider`, the one trait, with `chat`, `capabilities` and `catalogue`.
- `Reach`, separating where a model runs from which vendor made it, with `is_on_device` and
  `uses_local_credential` as two distinct questions.
- `ModelCapabilities` per model and reach, and `ChatRequest::needs` to find out what a
  provider would drop before you send anything.
- `Usage` with `UsageCoverage`, so a call nobody measured reports as absent rather than zero.
- Providers: Anthropic Messages API, any OpenAI compatible endpoint, and a local command
  line tool run as a subprocess.
- `Registry` and `PriceBook`, both carrying where their facts came from and when a person
  last checked them.
- `testkit`, a contract suite for providers written outside this crate.
- A dated Anthropic model table and price book, both refusing a row with no provenance.
- Examples: `ask`, `what_it_cost`, and `anything_openai_shaped`.

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

Retries, images, embeddings. See the README for what each one costs you.
