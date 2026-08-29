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

Streaming, retries, images, embeddings. See the README for what each one costs you.
