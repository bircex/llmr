# Roadmap

Where llmr is, what shipped in each release, and what is left. Written so somebody picking
this up cold can carry on without asking anybody anything.

Read [docs/DESIGN.md](docs/DESIGN.md) first if you are about to change something. It says
what was decided and why, and several of those decisions look wrong until you know the
reason.

## Where it stands

llmr is a service: one Docker image (`ghcr.io/<owner>/llmr`) with an OpenAI-shaped client API,
a management API a panel drives, and an encrypted SQLite database on a volume. It began as a
crate; it is **not published to crates.io any more**, and 0.1.0 is the last version there.

Everything below the "Next" section is the history of the engine, written while it was a
crate. It is kept because the reasoning still holds for the code; where it talks about
publishing, docs.rs or a public API, that is what the project did then.

| | |
|---|---:|
| Source | 22,880 lines across 50 files, the service included |
| Tests | 469 passing, all features · 270 on the default set |
| Distributed as | The Docker image, for amd64 and arm64, from `main` and from `v*` tags |
| CI on GitHub | every pull request, including building and starting the image |

The checks, as `CONTRIBUTING.md` lists them, run on the toolchain in `rust-toolchain.toml`:

```sh
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings
cargo test --all-features
cargo test
```

---

## Next

**Done, not yet tagged:** the service (#76), its documentation (#77), REST management with the
encrypted store (#78), usage with cost (#79), and the command line tools in the image (this
change). Before the first tag: bump `version` in `Cargo.toml`, turn
the changelog's Unreleased section into that version's, and tag. The release workflow and the
image's `X.Y.Z` tags have not run yet, so watch the first one.

In order:

1. **The product site** on GitHub Pages: what llmr is and how to run it, not the code.
2. Per caller limits: spending caps and rate limits, now that usage is recorded. The engine's
   `Budget` is per process lifetime, which a long running service cannot use as is.
3. An Anthropic Messages endpoint beside the OpenAI one, so prompt caching and thinking
   signatures can cross; Bedrock (needs a SigV4 signing transport); embeddings.
4. Stop sequences and `tool_choice`, which the engine's `ChatRequest` cannot express yet and
   the client API therefore refuses.
5. **Open:** signing a command line tool in with a subscription (Claude Pro or Max, ChatGPT)
   instead of an API key. Left out on purpose, see docs/DESIGN.md: it is a question about the
   vendors' terms for a shared router before it is a question about code.

---

## Phase 1: structure and the dependency floor · **done**

Two things that are cheap before publish and breaking after it.

**The tree separates what is shared from what is chosen.** `providers/api/` and
`providers/cli/` hold the machinery, which follows the reach because reach is what decides
how a model is spoken to. `providers/anthropic/`, `providers/openai/`, `providers/gemini/`
and `providers/bedrock/` hold the providers, which follow **who you reach and whose
credential pays** — the vendor for a first party API, the gateway for a gateway. That is what
a caller picks, and the same models turn up behind more than one of them: Anthropic's answer
over the Messages API, through Claude Code, and through Amazon. `chat/` is what a call is made of,
`cost/` is what it consumed and what that is worth. Everything else is flat, deliberately: a
directory holding one file is a directory that exists to look organised.

This does not make `Reach` a directory. It is a runtime value on `ModelCapabilities`, which
is the only form a caller can read before sending, and the only form that could ever have
answered "may this prompt go there".

**A provider writes a protocol, not a client.** `ApiProvider` does the transport, the
credential, the status codes and the error mapping. A vendor supplies `Protocol`: what URL,
what headers, what JSON goes out, what comes back. On the command line side `LocalCli` does
the spawning and the deadline, and a vendor preset is a program name, its arguments, and the
shape of what it prints.

**Adding this crate used to cost 105 crates and now costs 31.** The providers never needed
`reqwest`; only `from_env` did. Protocols and the bundled client are separate features.

---

## Phase 1b: reachability · **done**

`capabilities` said what a model could do and nothing said whether you could reach it, so the
only way to find out was to send a request and read the failure. That costs a call and it
happens in production.

`Provider::validate` answers `Access`: `Ready`, `Denied` or `Unknown`. Three rather than
two, because a network that was down while the check ran is not a provider that refused, and
a boolean collapses them. `Router::preflight` asks every route once at startup, reports, and
prunes nothing.

It cost the Anthropic provider a `catalogue()` implementation, which is what it now asks for
free, and the command line providers a `with_probe`. The contract suite checks both halves,
and `assert_a_bad_credential_is_denied` is a second entry point because the suite cannot
break your credential for you.

See [docs/DESIGN.md](docs/DESIGN.md) for the four decisions in it, and
`cargo run --example is_it_reachable` for what it prints.

---

## Phase 2: streaming · **done**

The largest gap, and the reason it is before publish rather than after: it changes the shape
of `Provider`, so doing it in 0.2 breaks every implementation written against 0.1.

### What to build

A second method on `Provider`:

```rust
async fn stream(&self, request: ChatRequest) -> Result<EventStream<'_>>;
```

with a default implementation that calls `chat` and yields the whole reply as one burst, so
an existing provider still compiles and still works. `EventStream` is a boxed
`futures_core::Stream`; `futures-core` is one crate with no dependencies of its own, and
`futures` proper would have pulled a combinator stack this crate has no use for.

An `Event` carries a delta rather than a whole message: text appended, a thinking block
opened, a tool call accumulating, the stop reason, and finally usage.

### The three things that will be got wrong

**Usage arrives at the end.** In a streamed call the token counts come in the final event,
not with the answer. A caller that reads usage from the first event gets nothing, and the
absent-not-zero rule then quietly reports a free call. The contract suite has to check that a
streamed call and a non streamed call to the same model report the same usage.

**Thinking signatures still have to survive.** A reasoning block assembled from deltas must
end up with its signature attached, or the conversation cannot be continued. This is the same
property as `tests/what_goes_on_the_wire.rs::anthropic_keeps_the_signature_on_a_thinking_block`,
one layer harder.

**A stream can fail halfway.** After some text has already reached the caller. That is a
different situation from a call that failed, and the `Event` type has to be able to say so.

### Providers

Anthropic and the OpenAI shape both speak server sent events. The command line providers
cannot, and should say so through the capability they already have rather than by failing at
the call.

### Done when

A streamed and a non streamed call to the same model produce the same text and the same
usage, the contract suite checks both, and a provider that only implements `chat` still
compiles and still answers.

---

## Phase 3: retries and observability · **done**

### Retries

A policy the caller configures and the router applies. What belongs here rather than in every
caller: this crate already knows which failures are worth repeating (`Error::is_retryable`)
and what the server asked you to wait (`Error::retry_after`), and a caller reconstructing both
from a message will get it wrong.

What stays the caller's is **whether a request is safe to repeat**, which is a question about
their request rather than about the failure. A timeout is retryable and may still leave you
paying for two answers.

Rules to keep:

- Honour `Retry-After` when the server sent one. A local timer that fires sooner turns a rate
  limit into a longer rate limit.
- Back off with jitter when it did not.
- Never retry `Auth`, `InvalidRequest`, `Refused` or `Unreadable`. Each returns the same
  answer the second time.

### Observability

`tracing` spans on every call, carrying provider, model, reach, usage coverage and which
route answered. Behind a feature, because a library that logs whether you asked or not is a
library people work around.

The span is also where the router's `fell_through` becomes visible: a successful call that
took the third route is a provider degrading while nothing is failing, and today that is only
in a struct nobody looks at.

### Done when

A scripted rate limit produces exactly the wait the server named, a refusal is never retried,
and a call with the feature off emits nothing.

---

## Phase 4: the pipeline, actually running · **done**

`.github/workflows/ci.yml` covers formatting, three clippy passes, docs with warnings denied
under two feature sets, tests on three operating systems, every feature built alone, and the
stated minimum Rust version. Every one of those passes locally.

**It runs. The diagnosis that used to be written here was wrong**, and the wrong part is
worth keeping because it is the interesting bit: this section said every job finished in three
seconds having done nothing, and blamed a private repository drawing on a blocked Actions
allowance. The repository is public, Actions is enabled, and every run had in fact executed
and gone red. Nobody had opened one.

What was actually failing: CI asked for `stable`, 1.98 landed on 2026-08-20 with a new
`clippy::manual_slice_fill`, and `Secret::drop` zeroed its bytes with a loop. The crate had
not changed. Six commands passing on a laptop running 1.97 said nothing about a runner
running 1.98, so **green locally was not a claim about anything**.

`rust-toolchain.toml` now pins the compiler, so the six commands mean the same thing in both
places, and raising it is a deliberate commit. A weekly `ahead-of-stable` job runs clippy on
whatever stable is now, so a bump waiting to be done is news on a Monday rather than a red
tick on somebody's unrelated pull request.

### Added since

- **A supply chain job.** `deny.toml` with an allowlist of licences, advisories denied and no
  blanket ignores, and duplicates warned. `cargo deny check` passes; it warns about `syn` 1
  and 2 and two `windows-sys` versions, both transitive and neither ours to fix.
- **A release workflow.** `.github/workflows/release.yml` fires on a `v*` tag, re-runs all
  eight checks against that commit, and refuses if the tag disagrees with `Cargo.toml` or the
  changelog has no section for it. It published to crates.io then; now it
  creates a GitHub release, and the image comes from the Docker workflow.
- **A packaging job and `cargo-semver-checks` on pull requests.** Both guarded the crate's
  published API, and both were removed with the gateway, when there stopped being one.
- **Issue and pull request templates, and a code of conduct.** The provider template asks
  which vendor *and* which reach, because those decide different things: the vendor decides
  the directory, the reach decides what it can carry.

### Done when

A green run exists on GitHub, on three operating systems, with a job identifier somebody can
open. **Open it.** The failure this section describes survived for as long as it did because
a red tick was read as the thing that was already known to be broken.

---

## Phase 5: 0.1.0 · **done**

### The public surface

Read it with fresh eyes and make anything that does not need to be public private, because
narrowing after publish is breaking and widening never is. Done once (#19); what it found:

- The provider helpers were already private. `read_block`, `wire_message`, `budget` and their
  neighbours are translation details and none of them was ever exposed.
- Four types that a caller reads or builds gained `#[non_exhaustive]`: `Priced`, `ToolSchema`,
  `Attempted` and `UsageNames`, with constructors for the two that callers build.
- `Priced` was the pointed one — it had no currency, so `Ledger::total` was adding dollars to
  euros and returning a number in neither. Now fixed: `Priced::currency`, `Ledger::total`
  returning `None` for a mixed run, and `Ledger::totals` for one figure per currency. The
  reading pass is what found it, which is the argument for doing the pass at all.
- Four crates are part of the public API and a major bump of any is a breaking change here.
  `docs/DESIGN.md` names them and what each costs.
- The count needed a method more than it needed a number.

Particular things to look at: `Protocol` and `Tool` are extension points and should stay;
helper functions inside the providers should not be public unless somebody outside would call
them.

### Then

`cargo publish`. Done for 0.1.0, which is the last version on crates.io: the project stopped
publishing a crate when it became a gateway.

---

## After 0.1

Not planned in detail, and roughly in this order.

| | Why it is not before 0.1 |
|---|---|
| ~~Gemini~~ · **done** | `providers::gemini::api`. Writing it found two holes in `Protocol`: `chat_url` had no model, and nothing could say the streaming URL differs |
| ~~Bedrock~~ · **done** | `providers::bedrock::api`, under the gateway as #29 decided, and the first use of `CloudPartner`. SigV4 is the transport's, not the protocol's |
| ~~Images~~ · **done** | `ContentBlock::Image`, refused rather than stripped where a reach cannot carry one |
| More CLI presets | Gemini CLI and whatever else appears (#24, #46). A preset is a file and it goes beside its vendor's other reaches — but it needs a recorded `--output-format json` sample first, because inventing the usage field names reports a number that looks right and is not |
| ~~Cost accumulation~~ · **done** | `cost::ledger::Ledger`, with a total that says when it is a floor, and refuses to be one number when the run mixes currencies |
| ~~Embeddings~~ · **done** | `embed`, behind a feature, as #26 decided. A vector carries the model that made it and `similarity` refuses across two — the currency rule in a different type. Two implementations, agreeing on nothing at the wire and passing one contract |

## After 0.1.0: twelve issues, and where each got to

Filed once 0.1.0 was cut, ordered by what an agentic layer that executes work and returns
what it cost actually needs. **Ten shipped in 0.2.0.** Two are finished as far as anything in
this repository can take them, and both are blocked on the same thing: a machine with a key or
a tool on it.

| | State |
|---|---|
| #37 No provider has ever met a real endpoint | **suite in, calls not made.** `tests/against_a_real_endpoint.rs` and a dispatched workflow exist. Nobody has run them with a key |
| #38 A CLI run's cost is always a floor | **done.** `UsageCoverage::Estimated`, `Total::About`, `Ledger::record_subscription`, `Provider::subscription`, `Ledger::summary` |
| #39 Ship dated registry and price tables | **done.** OpenAI and Gemini tables read off vendor pages, plus `PriceBook::age` and `needs_rechecking` so staleness is findable |
| #40 Budget | **done.** `Router::within`, refusing before the money goes, with what it cannot promise written down |
| #41 `Router::stream` | **done.** Falls through before the first event and never after it |
| #42 The router has no memory | **done.** `Breaker`, atomics, and a reason for every skip |
| #43 `preflight` answers and nothing reads it | **done.** `Denied` rests a route, `Unknown` still changes nothing |
| #44 Selection is list order and nothing else | **done.** `Order::Cheapest` and `Order::Healthiest`, with an unpriced route sorted last rather than free |
| #45 No deadline across the whole attempt | **done.** `Router::within_deadline`, answering `Error::Timeout` |
| #46 More command line presets | **half done.** Claude Code is now checked against a real recorded envelope, which found a stop reason bug. Codex and Gemini CLI still need a recorded run |
| #47 Bedrock has no way to be called | **done.** `docs/BEDROCK.md`, and the wrapper as a compiled doctest |
| #48 Hedging, and the ledger surviving it | **done.** `Ledger::record_cancelled`, and the decision written down |

### What is left, precisely

**#37 needs a key.** `ANTHROPIC_API_KEY=... cargo test --all-features --test
against_a_real_endpoint -- --ignored --nocapture`, or the `Against a real endpoint` workflow
with the secrets set. Set `LLMR_RECORD` and commit what comes back. Until somebody does,
"the providers work" is a claim nobody has tested, and that is the largest remaining risk in
the crate.

**#46 needs the tools.** One recorded run each:

```sh
echo "say ok" | codex exec --json
echo "say ok" | gemini --output-format json
```

Paste them into `tests/recorded/`, add a case to
`tests/what_a_command_line_tool_prints.rs`, and fix whatever the presets turn out to have
been reading wrongly. Claude Code's recording found one bug in three fields, so assume the
others have one too. Guessing is not an option: the numbers arrive, they look plausible, and
every cost report built on them is wrong.

**One thing the Claude Code recording turned up and nobody has decided about.** Its envelope
carries `total_cost_usd`, a figure the tool worked out itself. Nothing reads it, because
there is nowhere in `ChatResponse` to put a cost that did not come from a `PriceBook`, and
adding one is a larger decision than a preset: it would be a second source of truth for the
one number this crate exists to get right.

### What 0.2.0 also carried

Two things that were not in the original twelve and were found on the way.

**`#[non_exhaustive]` on the nine public types that can grow** (#70). The rule was written in
`docs/DESIGN.md` before 0.1.0 and applied to only some of the crate, and 0.2.0 paid for it:
`cargo-semver-checks` failed three lints, and the fix was a version bump rather than a change
to the code. It had to land in this release or cost a major bump of its own in the next one.

**Six stray conflict markers** that reached `main` in a botched branch split (#62), removed
before they could ship.

### The test debt, carried deliberately

Five gaps found by auditing which public items no executable test ever touches. All labelled
`test`, none of them blocking a release, all of them real:

| | |
|---|---|
| #67 | `Breaker` and `Budget` are the only mutable state and `tests/concurrency.rs` ignores both |
| #68 | Nothing notices when a shipped price table goes stale, including the one with an announced expiry |
| #71 | `Spawning` is the only `ProcessRunner` anybody uses and the only one with no test |
| #72 | `temperature` and `top_p` are written by three protocols and checked by no test |
| #73 | The `Retry-After` header path is untested, half implemented, and copied twice |

#72 and #73 are the two worth doing first. Both are the same shape as #46: a wrong key or an
unparsed header does not fail, it quietly drops what the caller asked for, and the reply looks
correct all the way down.

## Things known to be missing from the engine

Reranking and completion endpoints, audio and documents, and a model catalogue on the command
line providers, which cannot be asked what they serve. Bedrock does not stream, because its
event framing is not server sent events. The gateway's own gaps are listed in the README; if
you fix one, take it out of that list in the same commit.

Streaming, retries, images and embeddings used to be on this line and are not any more.
