# Roadmap

Where this crate is, what is left before 0.1.0, and what each phase needs. Written so
somebody picking this up cold can carry on without asking anybody anything.

Read [docs/DESIGN.md](docs/DESIGN.md) first if you are about to change something. It says
what was decided and why, and several of those decisions look wrong until you know the
reason.

## Where it stands

As of the vendor-first provider tree.

| | |
|---|---:|
| Source | 4,694 lines across 25 files |
| Tests | 124 passing |
| Public items | 180 |
| Dependency tree, default features | 52 crates |
| Published | no |
| CI on GitHub | has never run, see phase 4 |

Everything below is green locally: `cargo fmt --check`, clippy under three feature
combinations, `cargo doc` with warnings denied under both feature sets, every feature built
alone, and the full test suite.

```sh
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo test --all-features
```

Run all seven before any commit. The third one catches more than it looks like it should,
because a lint can fire under one feature set and not another, and the sixth is there for
the same reason: a doc link to a feature gated item resolves under `--all-features` and
nowhere else.

---

## Phase 1 — structure and the dependency floor · **done**

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

**Adding this crate used to cost 250 crates and now costs 52.** The providers never needed
`reqwest`; only `from_env` did. Protocols and the bundled client are separate features.

---

## Phase 2 — streaming · **next**

The largest gap, and the reason it is before publish rather than after: it changes the shape
of `Provider`, so doing it in 0.2 breaks every implementation written against 0.1.

### What to build

A second method on `Provider`:

```rust
async fn stream(&self, request: ChatRequest) -> Result<BoxStream<'_, Result<Event>>>;
```

with a default implementation that calls `chat` and yields the whole reply as one event, so
an existing provider still compiles and still works.

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

## Phase 3 — retries and observability

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

## Phase 4 — the pipeline, actually running

`.github/workflows/ci.yml` covers formatting, three clippy passes, docs with warnings denied
under two feature sets, tests on three operating systems, every feature built alone, and the
stated minimum Rust version. Every one of those passes locally.

**On GitHub every job has finished in three seconds with zero steps.** Nothing ran. The
repository is private, private repositories draw on the account's Actions allowance, and that
account is blocked for billing. Public repositories get unlimited free minutes and do not
touch the allowance, so making this one public is likely to fix it outright and is worth
trying before assuming anything else is wrong.

### Still to add

- **A supply chain job.** `deny.toml` with license, advisory and duplicate checks. This is not
  ceremony: the same check on a sibling project caught a yanked crate this week.
- **A release workflow.** A tag verifies and publishes. Publishing by hand from a laptop is
  how a crate ships from a dirty working tree.
- **`cargo-semver-checks` on pull requests.** After 0.1 there is a public API to break by
  accident.
- **Issue and pull request templates, and a code of conduct.** Small, and they are what a
  first contributor reads.

### Done when

A green run exists on GitHub, on three operating systems, with a job identifier somebody can
open.

---

## Phase 5 — 0.1.0

### The public surface

180 public items is a lot to promise. Read it with fresh eyes and make anything that does not
need to be public private, because narrowing after publish is breaking and widening never is.

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
| Gemini, Bedrock | Two native protocols the OpenAI shape does not cover. Each is a `Protocol` impl under its own vendor directory, and adds nothing breaking |
| Images and other input | `ContentBlock` gains a variant. It is `#[non_exhaustive]`, so this is additive |
| More CLI presets | Gemini CLI and whatever else appears. A preset is a file, and it goes beside its vendor's other reaches |
| Cost accumulation | `Usage::merge` exists; a ledger over a run does not. Additive |
| Embeddings | A different question from chat, and arguably a different crate |

## Things known to be missing, said in the README

Streaming, retries, images, embeddings, and model catalogues on everything except the OpenAI
shape. If you fix one, take it out of the README's list in the same commit.
