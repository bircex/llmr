# Roadmap

Where this crate is, what is left before 0.1.0, and what each phase needs. Written so
somebody picking this up cold can carry on without asking anybody anything.

Read [docs/DESIGN.md](docs/DESIGN.md) first if you are about to change something. It says
what was decided and why, and several of those decisions look wrong until you know the
reason.

## Where it stands

As of images and the ledger landing.

| | |
|---|---:|
| Source | 8,859 lines across 31 files |
| Tests | 231 passing |
| Public items | 6,128 all in, 1,209 hand written · see below |
| Dependency tree, default features | 31 crates |
| Published | no |
| CI on GitHub | runs, and is green as of phase 4 |

Everything below is green: `cargo fmt --check`, clippy under three feature combinations,
`cargo doc` with warnings denied under two feature sets, every feature built alone, and the
full test suite.

The public item count is two numbers because it needs a stated method, and used to be one
number twice with no method at all — this file said 180 and issue #19 said 189, and neither
could be compared to anything. Both come from:

```sh
cargo +nightly public-api --all-features
```

6,128 is every public item, dominated by the trait implementations `derive` writes. 1,209 is
that with derive-generated trait methods filtered out, which is roughly what a reader of the
docs meets. Either is fine. Using the same one next time is what matters, and after 0.1.0
`cargo public-api --diff` answers the better question anyway.

```sh
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo test --all-features
```

Run all seven before any commit, and run them on the toolchain in `rust-toolchain.toml`
rather than whatever a laptop happens to have. That file is the reason these commands mean
the same thing here as on a runner; without it they passed on 1.97 and failed on 1.98 for
months.

The third one catches more than it looks like it should, because a lint can fire under one
feature set and not another. The sixth is there for the same reason: a doc link to a feature
gated item resolves under `--all-features` and nowhere else, so the all-features pass alone
cannot see one that is broken.

---

## Phase 1: structure and the dependency floor · **done**

Two things that are cheap before publish and breaking after it.

**The tree separates what is shared from what is chosen.** `providers/api/` and
`providers/cli/` hold the machinery, which follows the reach because reach is what decides
how a model is spoken to. `providers/anthropic/` and `providers/openai/` hold the providers,
which follow the vendor because that is what a caller picks — and because the same models
turn up behind more than one reach, so putting the Messages API and Claude Code two
directories apart hid a choice rather than presenting it. `chat/` is what a call is made of,
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
  seven checks against that commit, refuses if the tag disagrees with `Cargo.toml` or the
  changelog has no section for it, and holds the publish behind a `crates-io` environment so
  a person approves it. A published version cannot be unpublished, only yanked.
- **A packaging job on pull requests.** `cargo package --list` and `cargo publish --dry-run`,
  so a packaging problem is found on a pull request rather than at the moment of release.
- **`cargo-semver-checks` on pull requests.** Nothing to compare against until 0.1.0 is
  published, and it is here now so the first release after it is checked by a job somebody
  already trusts rather than one added in a hurry. It does **not** skip on its own when the
  crate is unpublished — it exits 101 with "not found in registry", which is what it did the
  first time this job ran — so the job asks the sparse index first and says why it is
  skipping. A probe that cannot tell fails the job rather than guessing.
- **Issue and pull request templates, and a code of conduct.** The provider template asks
  which vendor *and* which reach, because those decide different things: the vendor decides
  the directory, the reach decides what it can carry.

### Done when

A green run exists on GitHub, on three operating systems, with a job identifier somebody can
open. **Open it.** The failure this section describes survived for as long as it did because
a red tick was read as the thing that was already known to be broken.

---

## Phase 5: 0.1.0 · **next**

### The public surface

Read it with fresh eyes and make anything that does not need to be public private, because
narrowing after publish is breaking and widening never is. Done once (#19); what it found:

- The provider helpers were already private. `read_block`, `wire_message`, `budget` and their
  neighbours are translation details and none of them was ever exposed.
- Four types that a caller reads or builds gained `#[non_exhaustive]`: `Priced`, `ToolSchema`,
  `Attempted` and `UsageNames`, with constructors for the two that callers build. `Priced` is
  the pointed one — it has no currency field yet, and it will need one.
- Four crates are part of the public API and a major bump of any is a breaking change here.
  `docs/DESIGN.md` names them and what each costs.
- The count needed a method more than it needed a number.

Particular things to look at: `Protocol` and `Tool` are extension points and should stay;
helper functions inside the providers should not be public unless somebody outside would call
them.

### Then

`cargo publish`, and only then does anything switch a path dependency for a version.

### Done when

docs.rs has built it, a fresh project can add it and make one call, and the README's first
example works copied straight out.

---

## After 0.1

Not planned in detail, and roughly in this order.

| | Why it is not before 0.1 |
|---|---|
| Gemini, Bedrock | Two native protocols the OpenAI shape does not cover. Each is a `Protocol` impl under its own top level node — Gemini under the vendor, Bedrock under the gateway, decided in #29 — and adds nothing breaking |
| ~~Images~~ · **done** | `ContentBlock::Image`, refused rather than stripped where a reach cannot carry one |
| More CLI presets | Gemini CLI and whatever else appears. A preset is a file, and it goes beside its vendor's other reaches |
| ~~Cost accumulation~~ · **done** | `cost::ledger::Ledger`, with a total that says when it is a floor |
| Embeddings | Decided (#26): a trait in this crate behind a feature, not a second crate. `docs/DESIGN.md` says what breaks under the other. Still to build |

## Things known to be missing, said in the README

Streaming, retries, images, embeddings, and a model catalogue on the command line providers,
which cannot be asked what they serve. If you fix one, take it out of the README's list in
the same commit.
