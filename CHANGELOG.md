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

### Not in this release

Streaming, retries, images, embeddings. See the README for what each one costs you.
