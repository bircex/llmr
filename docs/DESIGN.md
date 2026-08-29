# Design

What was decided and why. Several of these look wrong until you know the reason, which is
exactly why they are written down: the failure mode is somebody tidying one away.

Each section says what would break if it were changed. If you disagree with one, disagree
with the reason rather than the rule.

---

## What this crate is

One question: **how do I reach this model, and what did it cost.**

It is not an agent framework. No tool loop, no memory, no orchestration. It does not decide
what your work needs either: the router picks a provider that meets a set of requirements,
and deciding that a code review needs reasoning while a commit message does not is policy
over your own system.

That line is what keeps it useful to more than one program. A router that knew what a
security review was would be one only its author could use.

---

## Reach is a separate axis from provider

`Reach` says where a model runs. It answers two questions that are easy to confuse:

```rust
Reach::LocalCli.uses_local_credential()  // true
Reach::LocalCli.is_on_device()           // false
```

A vendor command line tool signs in on your laptop and still sends every prompt to the
vendor. Code that treats "the credential is local" as "the data is local" will send something
private to a third party and record it as safe.

**If this became one boolean**, that case would be wrong in the direction nobody notices,
because the wrong answer still looks like it worked.

---

## Capabilities belong to the pair of model and reach

The same model behind a command line tool usually cannot take a tool schema or return a cache
breakpoint. That is a fact about the reach, not about the model, so `capabilities()` answers
for the pairing.

`ChatRequest::needs().unmet_by()` lists what a provider will drop before anything is sent.

**Without it**, you find out by reading a reply that quietly ignored half of what you asked
for, and paying for it. Nothing in the reply says so.

---

## Whether you can reach it has three answers, not two

`Provider::validate` asks whether a request would be accepted, before there is a request.

```rust
Access::Ready              // checked, and nothing was found that would stop a call
Access::Denied { reason }  // the provider was asked and said no
Access::Unknown { why }    // it could not be established
```

`Unknown` is the answer a boolean loses, and it is the one that matters. A tool that is not
installed is denied. A network that happened to be down while the check ran is not. Both
become `false`, and the second one takes a working provider out of a router for a reason that
had cleared before anybody read the log.

It is the same rule as `Usage`. What nobody measured is absent rather than zero, and what
nobody established is unknown rather than denied.

**If this were a bool**, one flaky minute at startup would strike a healthy provider off the
list, and every line of the log would say the check ran.

---

## `validate` returns an answer rather than a `Result`

It cannot fail. Every way of failing to find out is `Access::Unknown`.

The alternative has two channels carrying the same meaning: an `Err(Transient)` and an
`Unknown { why }` both say nobody knows, and a caller then has to handle both, so it will
handle one. Deciding which failures are "could not check" and which are "no" is this crate's
job, because it is the crate that knows a 401 is settled and a 503 is not.

The mapping therefore lives in one place. A rejected credential, a missing program and a model
the vendor does not list are `Denied`. A timeout, a rate limit, a server fault, an
unreadable body and a provider with nothing free to ask are `Unknown`.

---

## A check that costs a call is a check nobody runs

`validate` may not send a billable request. A provider with a model list asks for the list. A
command line provider asks the program whether it is there. Nothing generates a token.

A preflight that spends money gets called once, then wrapped in a flag, then skipped.

Nothing caches the answer either. A credential rotates, a subscription lapses, an entitlement
is granted, and a validated-once flag is a claim about a moment that has passed. Providers
hold no state anyway, which is what makes this easy rather than tempting.

`Router::preflight` is where it belongs: once at startup, beside `unusable`, and not on the
path a request takes. Validating per call doubles every round trip to learn something that was
almost always true.

---

## `Ready` says what was checked, not what will happen

A check that costs nothing cannot prove everything, and how much it proves depends on the
reach.

An API provider that asked for the model list has established the credential and the
entitlement, because those are what that endpoint answers with. A command line tool that ran
and printed its version has established that it is installed, and nothing at all about the
login inside it, because no vendor tool offers a free way to ask. A `Ready` from a CLI
provider is therefore a weaker claim than a `Ready` from an API one, and
`LocalCli::with_probe` exists for a tool that does have a sign in check worth running.

**This is said out loud** because the alternative is a caller reading `Ready` as a guarantee.
It is the absence of a known blocker, which is all a free check can be, and it is still the
difference between finding out at startup and finding out in production.

A model the provider does not know is never `Ready`. That is the failure the contract suite
already caught once, where a provider claiming to know every model name turns a typo into a
real model.

---

## Usage that was never reported is absent, not zero

`Usage` fields are all `Option`, and `UsageCoverage` travels with them.

A subscription command line tool measures nothing. A zero written in its place becomes a free
call in every report that adds it up, and no amount of care downstream can recover the
difference between "nothing" and "nought".

The same rule reaches into pricing: `PriceBook::price` returns `None` for a call with no
usage, rather than a cost of zero. And there is deliberately **no price row for the local
command line reach**, because a rate applied to an invented token count produces a number that
looks like a receipt.

---

## A reply that cannot be read is an error, never an empty answer

A 200 with a body this crate cannot parse returns `Error::Unreadable`.

**If it returned an empty message instead**, a caller would carry on with nothing and call it
a success. A caller cannot tell an empty answer from a failure, and one of those means keep
going.

For the same reason `Unreadable` is not retryable. The provider answered. Asking again returns
the same body.

---

## Content blocks this crate does not model are kept verbatim

`ContentBlock::Opaque { kind, raw }` holds anything unrecognised and sends it back byte for
byte.

This was a real bug before it was a feature. The reader dropped what it did not recognise, and
for a redacted reasoning blob that is silent corruption: the provider checks the history you
send back against what it produced, so the turn *after* the one that dropped it is rejected,
and the failure arrives with nothing pointing at the cause.

`Opaque::answer_text()` returns `None`. It is not an answer and must never reach a screen.

---

## Reasoning is not an answer

`ContentBlock::answer_text()` and `ContentBlock::reasoning()` are separate methods.

There was one `text()` that returned both. **That is a method somebody uses to fill a screen
with the model's private working out**, and the mistake is invisible until a user sees it.

---

## `Thinking` has three states and none of them is "unavailable"

```rust
Thinking::Unset      // no opinion, whatever the model does by default
Thinking::Off        // do not reason
Thinking::On(Effort) // reason at this level
```

On some models reasoning is on by default and on others it is off, so collapsing "no opinion"
into "off" silently changes behaviour the moment a model id changes.

Whether a model *can* reason is `ModelCapabilities::thinking`. It is deliberately not a
variant here: a request says what you want and a capability says what is possible, and one
enum carrying both is two answers to two questions in one place.

`Effort` has five levels because vendors expose five. With three the top two are names a
caller can write and nothing can act on.

---

## A provider writes a protocol, not a client

`ApiProvider` owns the transport, the credential, the status codes and the error mapping.
`Protocol` is what a vendor supplies: what URL, what headers, what JSON goes out, what comes
back.

Before this, every provider repeated the same twenty lines with one word changed, and the
copies would drift the first time one was fixed. It also means two providers cannot disagree
about what a 429 means.

`Protocol` is a **type parameter rather than a trait object**, so the call is resolved at
compile time and there is no vtable on the path a request takes.

A protocol holds no state. Every method is a pure function over a request or a body.

---

## Nothing holds a lock across an await

Every provider is immutable once built. `chat` takes `&self`, and anything shared is either
immutable after construction or an atomic. One instance serves any number of concurrent calls
with nothing to contend on.

This is enforced rather than intended:

```toml
[lints.clippy]
await_holding_lock        = "deny"
await_holding_refcell_ref = "deny"
```

Two failure modes, and only one of them is loud. A lock held across an await deadlocks, which
hangs. A lock held *around* the call serialises it, which fails nothing and makes a hundred
concurrent calls take a hundred times as long, and nobody notices until production.
`tests/concurrency.rs` checks both, with a timeout, because a hanging test gets killed by CI
with no explanation.

---

## The library may not panic

```rust
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
```

lifted inside `#[cfg(test)]`, because a test that cannot panic cannot assert.

`unsafe_code` is forbidden, not denied.

A library that decides to stop the process has taken a decision that belonged to the program
using it.

---

## The router routes on three things and no others

What the request needs, where the data may go, and the order you gave. Two behaviours are
decisions rather than details.

**A privacy floor is a floor, not a preference.** `Requirements::on_device()` means a hosted
provider is never tried, even when every local one is down. The tempting implementation falls
back, and what that does is send a customer record to a vendor the first time something is
slow, while every log line says the call succeeded.

**A refusal stops.** When a model declines, the next one is not asked the same question. That
is shopping a policy decision around until something agrees, and it is what you get by
accident, because a refusal arrives looking like any other error.

`Routed::fell_through` carries what was tried first and why. A non empty list on a
*successful* call is a provider degrading while nothing is failing.

---

## Tables carry their provenance

`Registry` and `PriceBook` rows require a `source` and a `verified_at`, and parsing refuses a
row without them.

A table with no date on it is a set of claims, and the first time one number turns out to be
wrong there is no way to tell which of the others still hold.

`Priced` records which price book edition produced it, so **historical costs are never
recomputed**. Re-pricing the past when a price changes destroys the record.

`Registry::stale` and `Registry::unlisted` compare a table against what a provider says it
serves. Neither prunes: a row that vanished because a vendor retired a model is a decision
somebody should make.

---

## Money is integers

`Micros` is millionths of the currency unit. Prices are written as decimal text and parsed,
never as floats, because `0.1` is not a value a binary float holds exactly and a column of
them drifts.

More than six decimal places is refused rather than rounded. A price written to seven is one
copied from a different unit, and the rounded version looks correct while being wrong by a
factor of ten.

`Micros::exact()` writes six places rather than two. Rounding a per call cost to cents turns
most calls into zero, and a column of zeros adds up to nothing.

---

## Public structs are `#[non_exhaustive]`, so they have constructors

Fields can be added without a major version, which also means outside code cannot build one
with a literal.

**Every type a provider or transport implementer has to construct therefore has a
constructor.** That gap was found by writing the tests as an outside caller: before the
constructors existed, "write your own provider" did not compile, and nothing inside the crate
would have noticed.

If you add a struct outside code must build, give it a constructor in the same commit.

---

## The contract suite is applied to the crate's own providers

`testkit::assert_provider_contract` is behind a feature for outside users, and
`tests/every_provider_honours_the_contract.rs` runs it against all three of ours.

A suite only outsiders have to pass is a suite nobody inside is held to. It has already earned
this: applying it caught the command line provider claiming to know every model name, which
turns a typo into a real model.

`assert_a_bad_credential_is_denied` is a second entry point rather than part of the main
suite, because the suite cannot break your credential for you: only you can build the
provider with the wrong key. It is worth the extra call it asks of you. A provider that
reports a rejected key as `Unknown` reads as "ask again later", so a router keeps that
provider and a retry loop keeps trying it, and the one failure a person has to fix is the one
that never surfaces.

---

## Features exist so a build compiles what it uses

| Feature | Crates | |
|---|---:|---|
| `anthropic`, `openai` | 52 | Both protocols. You supply the transport |
| `+ reqwest` | 250 | And a bundled client, with `from_env` |
| `cli` alone | 53 | A local tool as a subprocess, no network code |

The first two are on by default and `reqwest` is not. Almost every program already has an HTTP
client, and adding this crate should not add two hundred more.

Examples are gated with Cargo's `required-features` rather than a `cfg` attribute inside the
file, so `cargo run --example` reports the missing feature instead of building a binary with
nothing in it.

---

## Naming and prose

Tests are named after the claim they make, not the function they call.
`a_reply_with_no_readable_content_is_an_error_not_an_empty_answer` tells a reader what breaks
if it fails; `test_chat` does not.

Comments say why, not what. If a decision would look wrong to somebody reading it cold, the
comment says what would break without it.

No em dashes anywhere, in code or prose.
