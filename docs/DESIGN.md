# Design

What was decided and why. Several of these look wrong until you know the reason, which is
exactly why they are written down: the failure mode is somebody tidying one away.

Each section says what would break if it were changed. If you disagree with one, disagree
with the reason rather than the rule.

---

## What llmr is

A service (`src/bin/llmr/`) in front of an engine (the rest of `src/`). The service is what
people run: one Docker container with an OpenAI-shaped client API, a management API a panel
drives, and an encrypted database on a volume. The engine is how it reaches providers and
chooses between them. Most sections below are engine decisions, because that is where most
of the decisions are; the service's own are collected near the end.

Both answer one question: **how do I reach this model, and what did it cost.**

It is not an agent framework. No tool loop, no memory, no orchestration. It does not decide
what your work needs either: the router picks a provider that meets a set of requirements,
and deciding that a code review needs reasoning while a commit message does not is policy
over your own system. In the gateway, that policy is the operator's: which names exist and
which routes serve each.

That line is what keeps it useful to more than one program. A router that knew what a
security review was would be one only its author could use.

---

## Reach is a separate axis from provider

`Reach` says where a model runs: the vendor's own API, a cloud partner, a private endpoint
you control, or weights you run yourself. The provider says who you are talking to; the reach
says where the prompt ends up.

```rust
Reach::FirstPartyApi.is_on_device()  // false
Reach::SelfHosted.is_on_device()     // true
```

The same protocol serves more than one reach. An OpenAI-shaped endpoint may be a hosted API
or a model on your own hardware, and nothing in the wire format says which. Code that decides
where a prompt may go by looking at the protocol will send something private to a third party
and record it as safe.

**If reach were folded into the provider**, that case would be wrong in the direction nobody
notices, because the wrong answer still looks like it worked.

---

## Capabilities belong to the pair of model and reach

The same model behind a cloud partner or a self hosted server often cannot take everything
its vendor's own API takes: a tool schema, a cache breakpoint, an image. That is a fact about
the reach, not about the model, so `capabilities()` answers for the pairing.

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

`Unknown` is the answer a boolean loses, and it is the one that matters. A key that was
rejected is denied. A network that happened to be down while the check ran is not. Both
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
handle one. Deciding which failures are "could not check" and which are "no" is the engine's
job, because it is the crate that knows a 401 is settled and a 503 is not.

The mapping therefore lives in one place. A rejected credential and a model the vendor does
not list are `Denied`. A timeout, a rate limit, a server fault, an
unreadable body and a provider with nothing free to ask are `Unknown`.

---

## A check that costs a call is a check nobody runs

`validate` may not send a billable request. A provider with a model list asks for the list.
Nothing generates a token.

A preflight that spends money gets called once, then wrapped in a flag, then skipped.

Nothing caches the answer either. A credential rotates, an account lapses, an entitlement
is granted, and a validated-once flag is a claim about a moment that has passed. Providers
hold no state anyway, which is what makes this easy rather than tempting.

`Router::preflight` is where it belongs: once at startup, beside `unusable`, and not on the
path a request takes. Validating per call doubles every round trip to learn something that was
almost always true.

---

## `Ready` says what was checked, not what will happen

A check that costs nothing cannot prove everything.

An API provider that asked for the model list has established the credential and the
entitlement, because those are what that endpoint answers with. It has established nothing
about whether the account has credit left, whether a rate limit is about to be hit, or
whether this particular request will be accepted, because no vendor offers a free way to ask.

**This is said out loud** because the alternative is a caller reading `Ready` as a guarantee.
It is the absence of a known blocker, which is all a free check can be, and it is still the
difference between finding out at startup and finding out in production.

A model the provider does not know is never `Ready`. That is the failure the contract suite
already caught once, where a provider claiming to know every model name turns a typo into a
real model.

---

## Usage that was never reported is absent, not zero

`Usage` fields are all `Option`, and `UsageCoverage` travels with them.

Some providers report nothing: a self hosted server that leaves `usage` out, a stream cut off
before its last event. A zero written in its place becomes a free call in every report that
adds it up, and no amount of care downstream can recover the difference between "nothing"
and "nought".

The same rule reaches into pricing: `PriceBook::price` returns `None` for a call with no
usage, rather than a cost of zero, because a rate applied to an invented token count produces
a number that looks like a receipt.

---

## An estimate is neither measured nor a floor

`Usage::absent` says a call was not measured, and a `Ledger::total` containing one is a
floor. Both are right. What they leave out is a caller who counted the tokens themselves.

**A counted token is not a reported one.** `UsageCoverage::Estimated` exists so a locally
counted number can be added up without being folded into `Exact`, which would destroy the one
property the type is for. It is not `Partial` either: partial understates, so a partial total
is a floor, while an estimate can be wrong in **either** direction. That is why `Total::About`
outranks `Total::AtLeast` when both apply. A lower bound that can be false is worse than an
honest approximation, and `Ledger::estimated` keeps the guessed part findable after it has
been added to the measured part.

**The engine does not count the tokens.** A tokeniser has to match the vendor's, per model,
and one that is close produces numbers that look right and are not. `Usage::estimating` takes
a count the caller produced. Bringing a tokeniser in would be the engine manufacturing
exactly the confident wrong number every type in `cost` exists to prevent.

`Ledger::summary` is the sentence with all of it in: what was measured, what was estimated
and what has no figure. It exists because every program that assembled that from the
accessors would leave one out.

---

## A reply that cannot be read is an error, never an empty answer

A 200 with a body the engine cannot parse returns `Error::Unreadable`.

**If it returned an empty message instead**, a caller would carry on with nothing and call it
a success. A caller cannot tell an empty answer from a failure, and one of those means keep
going.

For the same reason `Unreadable` is not retryable. The provider answered. Asking again returns
the same body.

---

## Content blocks the engine does not model are kept verbatim

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

## The module tree groups by who you reach; the shared machinery groups by reach

`providers::anthropic::api`, `providers::openai::api` and `providers::gemini::api` are what a
caller imports. `providers::api` is what a contributor builds on.

This was the other way round once — `api::anthropic` and so on — on the reasoning that reach
is the difference that matters. Reach *is* the difference that matters, and that turned out
to be an argument for something else.

**What is shared follows the reach.** Everything an API provider does apart from writing JSON
is identical. That is why `ApiProvider` exists and why it sits under `api/`. Reach is the axis
the *code* is organised by, and it still is.

**What is chosen follows the vendor.** A caller knows which vendor before they know which
reach, and the same models turn up behind more than one: Anthropic's over the Messages API
and through Bedrock, and those differ in what they can carry rather than in what they are.
Reach-first put that comparison directories apart, and a caller weighing one against the
other could not see there was a choice.

**If this became reach-first again**, the vendor files would have to move but nothing would
break, because the engines are not in them. The cost is paid by the reader, not the compiler,
which is exactly the kind of cost that goes unnoticed until somebody sends a prompt somewhere
because they never saw the alternative beside it.

### What this does not mean

It does not put reach in the type system's back seat. A module path is read once, by whoever
writes the import; `Reach` has to be readable by a program deciding at runtime whether a
prompt may go somewhere. That is why it lives on `ModelCapabilities` and always did. The
directory layout never answered that question and never could.

`providers::openai::api` is the one name wider than what it holds: it serves Groq, vLLM,
Ollama and the rest. It sits there because the shape is OpenAI's and that is what everyone
calls it. It is also the one provider whose reach is a constructor argument, for the reason
in the section above — and the module header says so, because a name that is nearly right is
worse than one that is obviously approximate.

### The top level is who you reach, not who made the model

This started as "group by vendor", and the first gateway broke it: Bedrock serves Anthropic,
Meta and Mistral models over one API with one credential, and it is not a model vendor at
all. Three options were on the table (#29):

1. `providers::bedrock::api` — a top level node per gateway.
2. `providers::anthropic::bedrock` — under each vendor whose models it serves.
3. A third top level group, beside the vendors and the machinery, just for gateways.

**Option 1, and the rule is now stated properly**: the top level names **who you reach and
whose credential pays**, which for a first party API happens to be the vendor and for a
gateway is the gateway. Nothing moves; the sentence gets more accurate.

That reading was always the real one. It is why `openai::api` takes its reach as a
constructor argument — point it at Ollama and you are reaching your own machine, so the
module cannot answer the question and asks instead.

**Option 2 is the one to argue with, because it is the friendly one.** A caller looking for
Claude on Bedrock will look under `anthropic` first, and option 2 is where they would find
it. It is rejected because it makes `anthropic::api` and `anthropic::bedrock` read as two
routes to the same place, and they are not: different endpoint, different credential,
different company holding your prompt. The engine exists to keep that distinction legible,
and burying it one level down in the directory that says "Anthropic" is exactly the
collapse `Reach` was separated from `Provider` to prevent. It would also mean one `Protocol`
impl copied into several vendor directories, or re-exported from them, which is the same
lie told twice.

Option 3 was rejected for costing every reader forever: a tree with two kinds of top level
node has to be explained before it can be used, and the explanation is longer than the
problem.

**What this costs**, and it is a real cost: discovery. Somebody wanting Claude on Bedrock
looks under `anthropic` and does not find it. The mitigation is a line in each vendor's
`mod.rs` naming the gateways that also serve it — cheap, and it puts the pointer exactly
where the person is already looking.

**If option 2 were adopted later**, nothing would fail to compile and the first prompt sent
to AWS by somebody who thought they were talking to Anthropic would not fail either. It
would simply be wrong, in the direction the engine exists to catch, and no test could see
it.

---

## Embeddings are a trait here, not a second crate

Embeddings are a different question from chat: different request, different reply, different
usage shape, no messages, no stop reason, no reasoning, no tools. Almost nothing in `chat/`
applies (#26).

**They belong in the engine, as their own trait, behind a feature.** Not as a method on
`Provider`: adding `embed` there would make every chat-only provider implement a refusal,
which is a worse tax than the one being avoided.

The question was whether they belong here at all or in a crate depending on this one. The
argument for a separate crate is that everything a *caller* touches is unshared. The
argument that wins is that everything a caller **relies on** is shared, and it is the half
that took longest to get right: `Reach`, `Error` and its retry advice, `Usage` with its
absent-is-not-zero rule, `Registry` and `PriceBook` with their provenance, and the transport
boundary. An embedding call has a reach, costs money, and can go unmeasured, and every one
of those answers should be the same answer.

**What breaks under a separate crate**: the two must move in lockstep, because a `Usage`
from version A is not a `Usage` from version B. A caller doing both chat and embeddings
would hit "expected `Usage`, found `Usage`" the first time the versions drifted, and the fix
would be a coordinated release every time either crate changed. That is a permanent tax paid
by the people using both, to save a feature flag from the people using one.

**If this is reversed**, the moment to do it is before anything is published. Afterwards it
means yanking a feature, which is a breaking change dressed as a tidy-up.

### A vector belongs to the model that made it

`Embedding` carries the model that produced it, and `Embedding::similarity` answers `None`
rather than a number when asked to compare across two of them.

This is the currency rule again, in a different type. Two vectors of the same length from two
models occupy unrelated spaces; cosine similarity computes happily and returns a confident
number between -1 and 1 that means nothing at all. Every operation anybody performs on the
result — clustering, a nearest neighbour index, a relevance threshold — works perfectly and
is wrong. **The failures worth designing against are the ones that produce a plausible answer
rather than an error**, and the engine now has two of them written down.

### The reply is index for index with the request

Several vendors send an `index` on every row precisely because their arrays carry no order.
A provider that trusts arrival order pairs every document with another document's vector, the
index builds, the queries run, and the results are quietly wrong.

So it is a contract rather than a convention: `testkit::assert_embedder_contract` embeds each
input alone and checks it lands nearest the batch vector at its own position, and
`tests/an_embedder_honours_the_contract.rs` runs it through an endpoint double that reverses
every reply. A suite a broken implementation passes is worse than no suite, so there is a
`#[should_panic]` test holding a deliberately broken embedder against it.

### Two embedders, because one is a description

`providers::openai::embed` and `providers::gemini::embed` both ship, and the second is there
for what it disagrees with rather than for the vendor. They differ on every one of the three
things the module makes claims about:

| | OpenAI shape | Gemini shape |
|---|---|---|
| `Purpose` | nowhere to put it | `taskType`, so `capabilities.purposes` is true |
| Order | an `index` per row, sorted by it | no index; array order is the promise |
| Usage | `prompt_tokens` | none at all, so every call is `absent` |

Both pass `testkit::assert_embedder_contract` unchanged. **A suite one implementation passes
is a description of that implementation**, and the trait was written before either existed,
so this is the check that it is a specification instead.

The `Purpose` half also closed a gap the first release opened: an enum shipped in the public
API that nothing anywhere wrote to a wire. A caller setting it got the same vector either way
with nothing saying so — which is why `EmbeddingCapabilities::purposes` exists rather than a
promise that every reach honours it.

### `Usage::embedding` is a claim, and it is stated

An embeddings endpoint reports prompt tokens and nothing else, because text goes in and a
vector comes out and a vector is not tokens. Left as one field of four, `coverage()` would
read `Partial` for a call that was measured exactly, and one embedding anywhere in a run would
turn every `Ledger` total into a floor for good.

So `Usage::embedding` sets the other three to zero and reads `Exact`. That is the claim
`prompt_tokens` already makes — a provider reporting some fields and not others is saying the
others did not happen — and here they did not. A vendor that does report cached tokens on an
embedding call uses the builders instead.

---

---

## A stream is the same reply, and has to prove it

`Provider::stream` exists beside `chat` rather than replacing it, and the default
implementation calls `chat` and hands the whole reply over as one burst of events.

**That default is an answer, not a refusal.** A provider that cannot really stream still
answers `stream` with the same text and the same usage, all at once. The alternative — an
`Unsupported` error — would push every caller into writing the fallback themselves, and they
would each write it slightly differently.

Whether a pairing *really* streams is `ModelCapabilities::streaming`, read before the call
like every other capability. A provider that answers with one JSON document when it finishes
cannot stream whatever model is behind it, and it says so rather than failing.

The contract suite checks a streamed and a whole call agree about usage coverage. Two ways to
ask the same question that disagree about what it cost make every cost report depend on which
one happened to be used, and nothing in the report says which.

**If the default were removed**, adding `stream` would break every provider written against
`chat`. That is the reason this landed before publish rather than after: it is the shape of
the trait, so afterwards it is a version rather than a patch.

### Interrupted is a stop reason

A stream that ends without one arrives as `StopReason::Interrupted` rather than through a
separate channel. It is not something a provider reports, which is the argument against
putting it here — but `is_complete()` is the guard callers already check before using the
text, and a parallel channel is one they can forget to look at while rendering half an
answer as finished.

`Transcript::drain` returns the error and leaves the transcript intact, so what arrived, that
the turn did not finish, and why are three separate answers rather than one inferred from
another.

---

## A retry policy is handed in, never assumed

`Router` retries nothing until you call `retrying`. The crate knows which failures are worth
repeating and what the provider asked you to wait; it does not know whether **your** request
is safe to send twice, and that is the half that decides.

`Error::Timeout` is the case that makes this concrete. It is retryable — the failure was not
your fault and not permanent — and repeating it can still leave you billed for two answers,
because the deadline passing does not stop the provider generating. So it is excluded by
default and `repeating_timeouts()` turns it on.

**A wait the provider named is used exactly**: no jitter, no doubling, no ceiling. Capping it
would be a local timer firing before the limit clears, which earns a second 429 and a longer
wait. Waits the engine computes itself are jittered, because two callers that failed together
coming back together is how a provider recovering from a fault gets knocked over again.

**Jitter without `rand`.** A dependency on `rand` to spread retries apart would cost more
crates than the whole OpenAI protocol. Nothing here is a secret, so the clock's nanoseconds
through an xorshift are enough. If this ever needs to be unguessable rather than merely
uneven, that is a different requirement and it should arrive with its own reason.

**If retrying became the default**, the first timeout in somebody's production run would
double a bill, and the line that did it would not appear in any diff.

---

## Spans carry facts, and structurally cannot carry content

Behind the `tracing` feature. The gateway turns it on and writes the spans to stdout; with it
off, as in a build of the engine alone, there is no dependency and no work.

The rule is that a span never holds a prompt or a credential. That is not enforced by review:
every function in `observe` takes a `ModelId`, a `Reach`, a `UsageCoverage`, a count or a
route name, so there is nowhere to pass a message even by accident, and `Secret` has no
`Display`. `tests/what_a_span_says.rs` puts a known string into both the prompt and the key
and asserts it appears in no recorded field.

The span is attached to the future rather than entered around it. A span guard held across an
await attaches the span to whatever else that thread picks up next, which is the same class
of mistake as holding a lock across one.

**If the fields became a formatted string**, the first person who wanted a bit more context
would interpolate the request into it, and every program that upgraded would start logging
its users' text.

---

## The engine is not published, on purpose

Until the gateway, this was a crate on crates.io, and two sections here were about keeping
that promise: which dependencies leaked into its public API (`serde_json`, `serde`,
`futures-core`, `reqwest`), and how its public surface was counted with
`cargo public-api`. CI ran `cargo-semver-checks` and a `cargo publish --dry-run` on every
pull request.

All of that is gone, because the promise is gone. llmr ships as a Docker image and nothing
else, `Cargo.toml` says `publish = false`, and the engine's Rust types are how the service is
built rather than an API somebody downstream compiles against. What users depend on now is
the client API, the management API, the headers, the environment and the database on their
volume, and that is what `CONTRIBUTING.md` holds to semantic versioning. Rustdoc went with
the crate: it documented an API nobody outside calls. Documentation is for the product.

**Why not keep publishing both.** Two products with two compatibility promises is twice the
release work for a project with one maintainer, and every engine change would have to be
weighed against callers nobody can see. A Rust program that wants the router in-process can
still depend on this repository by git and pin a commit; it just gets no promise between
commits.

**What stays.** The engine keeps `#[non_exhaustive]` and constructors on its types, its doc
comments, and the doctests inside them. Those were never only about outside callers: they are
what makes a change to a type local, and the comments are how a contributor reads the code.

---

## Signing is a transport concern, not a protocol one

Bedrock authenticates with SigV4 rather than a bearer token, and #22 asked where that
belongs. It belongs in [`HttpTransport`], and the engine ships no implementation of it.

**A signature covers what a protocol cannot see.** SigV4 signs the method, the path, the
query, a set of headers and a hash of the body. A `Protocol` writes JSON and has no idea what
URL or headers the shared machinery is about to attach — by design, because that is what lets
one `ApiProvider` serve every protocol.

**It also needs state a protocol is not allowed to have.** A signature covers a timestamp, so
signing needs a clock; it covers a region; and it needs credentials that rotate. Every
`Protocol` method is a pure function, which is what makes one instance safe across any number
of concurrent calls. A signer with a clock and a credential store inside it would end that.

So `providers::bedrock::api` has **no key argument**. The credential belongs to the transport
you supply, and the protocol's `headers` deliberately attaches none — a bearer token beside a
signature is at best ignored and at worst a request Bedrock rejects.

**Why no SigV4 here.** Writing one would mean a crypto dependency and an implementation
nobody could test against the real thing from inside the engine. `aws-sigv4` exists, most
programs reaching Bedrock already have `aws-config` for credentials, and wrapping a transport
is ten lines. The crate already makes this bargain for HTTP itself: `reqwest` is a feature,
not a requirement.

**If signing moved into `Protocol`**, every protocol would gain a method that all but one
ignores, `ApiProvider` would have to hand it the request it is about to send, and the purity
that makes protocols shareable would go. The one gain would be that Bedrock needed no wrapper,
which is the smallest of the three things at stake.

**A principle that leaves a feature unusable needs a worked example beside it**, and for a
while this one did not have one. Enabling `bedrock` gave you a translation and a wall.
`docs/BEDROCK.md` is the page: what has to be signed and in what order, why the wrapper is
the last thing to touch the request, what the colon in a Bedrock model id does to a canonical
URI, where the region comes from, and why credentials that rotate belong in the transport.
The wrapper itself is a compiled doctest on `providers::bedrock`, over a `Signer` trait the
reader implements, so the half that touches the engine's API cannot rot while the half that
touches AWS's stays prose.

---

## Bedrock reuses the Messages translation rather than copying it

`providers::bedrock::api` calls `Messages.body()` and `Messages.read()`, then makes two
documented changes: the model moves from the body to the path, and `anthropic_version` takes
its place.

Claude through Amazon and Claude direct must send the same request. Two copies of that
translation would disagree the first time one was fixed, and the difference would surface as
a behaviour change on one route that nobody asked for and no test compared. There is a test
that sends the same request both ways and asserts the bodies are equal apart from those two
fields.

The cost is a feature dependency: `bedrock` requires `anthropic`. That is honest — it is the
Anthropic schema, spoken at a different address.

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

## Nothing panics, in the engine or the gateway

```rust
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
```

lifted inside `#[cfg(test)]`, because a test that cannot panic cannot assert.

`unsafe_code` is forbidden, not denied.

A process that dies on one malformed reply takes every other request in flight down with
it. The same lints are denied in the gateway binary, so a bad body from one provider is an
error for one request rather than an outage for all of them.

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

### A budget refuses before the money goes, and says what it cannot promise

`Ledger` records what a run cost and nothing stopped it. `Router::within` is the cap, and
three things decide whether it means anything.

**It is checked before a request goes out.** A cap checked after the call is a report. What
can actually be checked beforehand is two things, and both are real numbers rather than
guesses: whether anything is left, and whether the reply alone could overrun what is left.
The second needs `max_tokens`, and it is an upper bound, which is what makes it safe to
refuse on: no reply is longer than the limit it was given.

**The prompt is not priced, because that needs a token count.** Same decision as everywhere
else: a tokeniser that is close produces numbers that look right and are not. So a budget is
a cap on what a run may *start*, and the last call can carry it over by whatever the prompt
cost. Said in the docs rather than implied, because a cap that quietly does not hold is worse
than no cap.

**An unpriced route is refused by default.** A route with no price book cannot be measured
against a cap at all, and running it would break the one promise the budget makes without
anything noticing. `Budget::allowing_unpriced` is how a caller says otherwise, out loud, and
what comes back is a `Spending::unmeasured` count that makes `spent` read as the floor it is.

**Another currency is refused, not converted.** `Ledger::total` already answers `None` for a
mixed run, and for the same reason: an exchange rate has a date and a source exactly like a
price, and inventing one produces a figure nobody can audit.

**`Error::OverBudget` is its own variant.** Not the last provider error, because there is no
provider error: nothing was sent and nothing was billed. It opens no circuit either, for the
same reason. Adding it forced that decision, which is what the missing wildcard arm in
`Breaker::opening_for` is for.

**Two races are admitted rather than hidden.** Concurrent tasks can both see room for one
call and both spend it, because holding a reservation across the call means holding a lock
across an await. And a streamed call cannot be charged by the router at all, because its
usage arrives in a `Transcript` the router never sees: `Router::stream` counts one as
unmeasured, and `Router::charge` is how the caller settles it against the same book.

---

### A deadline bounds the whole attempt, and says so by its type

A transport can have a timeout and a `Retry` policy can have delays, and nothing bounded the
sum. Three routes and a policy of two attempts each is six timeouts plus the waits between
them, so a caller waiting on an agent had no way to say "answer or fail within twenty
seconds". `Router::within_deadline` is that bound.

**Checked before every attempt and before every retry wait**, not only at the start. A
`Retry` policy that says three attempts is a maximum rather than a promise, so when the two
disagree the deadline wins: a wait that would run past it is not taken.

**There is no minimum attempt length.** Any time left at all is enough to try again, because
a minimum would be the engine guessing how long a call takes, and a guess that stopped an
attempt which would have finished is worse than one attempt that overruns.

**It cannot cut short a call already in flight.** Cancelling one needs a timer, which needs a
runtime, and the engine does not pick yours. The deadline bounds when a new attempt starts
and how long the router waits between attempts; the length of a single call is the
transport's timeout, and both need setting.

**The answer is `Error::Timeout`, not the last error from a route.** A call that gave up has
to say whether everything failed or time ran out. One of those is a provider to look at and
the other is a deadline to raise.

`Routed::fell_through` is the obvious place for "the deadline was spent after two attempts"
and it is the wrong one: a `Routed` only exists when a reply came back, and a spent deadline
never produces one, so the entry would be written and never read. What was tried goes on the
span instead, under the `tracing` feature.

---

### Selection can be more than list order, and an unpriced route is not a free one

The only policy was the order you wrote, while the crate held `PriceBook`, `Rate`, `Micros`
and `Ledger` and consulted none of them when choosing a route. `Order` is the axis, and
`Order::AsListed` is still the default and still what the router always did.

**The requirement check comes first, whatever the ordering says.** Cheapest among the routes
that can serve the request, not cheapest full stop. Reordering past a capability would pay
less for a reply that ignored half of what was sent.

**`Order::Cheapest` claims something about the rate and says so.** It compares the sum of a
route's published input and output price per million tokens. It is not a prediction of what
a request will cost: cheapest per token is not cheapest per request, because a model with a
low rate that thinks before answering can spend more output than a dearer one that does not.
What bounds spend is a cap, not an ordering.

**An unpriced route sorts last, never first.** `PriceBook::price` answers `None` for a model
it does not list, and the obvious implementation reads `None` as zero and puts every unpriced
provider at the front, which is precisely backwards and confidently so. The sort key leads
with `rate.is_none()`, and `false` sorts before `true`. A route that carries a whole book
with no row for its model is unpriced too, which is the same fact as having no book.

Sorted last rather than refused. Refusing is a different decision and belongs to whatever
sets a spending limit, because a cap is the thing that cannot be satisfied by a call nobody
can price.

**`Order::Healthiest` works without a breaker.** The consecutive failure count is kept
whatever the policy, because incrementing an integer costs nothing and the count is what this
sorts on. What a `Breaker` adds is skipping a bad route entirely for a while rather than
merely putting it further down the list.

**Every sort is stable**, so routes an ordering cannot tell apart keep the order they were
written in, and a router where nothing has failed and nothing is priced behaves exactly like
the one people already have.

---

### A router that does not learn is an ordered `try` list

`Router::route` started at route 0 on every call, so a provider that had been answering 503
for ten minutes was tried first, waited on, and fallen through, for every single request.
`Router::breaking` gives the router a memory.

**Atomics, not a lock.** `chat` takes `&self` so one router can be shared across as many
tasks as a program has, and the engine denies holding a lock across an await crate wide. Two
tasks racing to record a failure are recording the same fact, so the race does not matter.

**Monotonic, not the wall clock.** A circuit needs two times compared. `Instant` is captured
when the router is built and everything is measured from it, so a machine whose clock steps
backwards cannot leave a route closed for a day.

**One rule about which failures count: the circuit opens for a failure about the provider and
stays shut for a failure about the request.** A refusal is the model answering about the
work; an invalid request will be malformed on the next route too. Closing a circuit for
either removes a working provider for something it had nothing to do with. `Breaker`'s table
lists all nine variants, and there is no wildcard arm: a variant added later stops the crate
compiling until somebody decides which side it falls on.

`Error::Unreadable` is the one worth arguing about, and it does not open the circuit. The
provider answered; a single reply the engine could not parse is as likely to be one odd body
as a provider gone bad, and the call falls through to the next route either way.

**The circuit is told once per request, not once per attempt.** A `Retry` policy asking three
times against a provider that is down is one piece of evidence, not three.

**Nothing reopens a circuit, because time does.** There is no half open state and nothing to
call. A route whose wait has passed is simply tried, and answering clears both the timer and
the count: leaving the count would make the next failure back off as though the run of
successes in between had not happened.

**A skipped route is never skipped silently.** It goes into `fell_through` with how much
longer it is resting and how many requests it has failed, and `Router::resting()` answers the
same without making a request. The wait is reported as a duration rather than a wall clock
time because formatting one would mean a date dependency the engine does not have, and
"another 4200ms" is the number somebody acts on anyway.

**When every route that could serve a request is resting, the error is `Transient`, not
`Unsupported`.** Nothing is wrong with the request, and the difference decides whether a
person fixes their configuration or waits.

**Handed in, never assumed**, exactly like `Retry`. Not trying a provider is a decision with
consequences the engine cannot weigh on its own, and a router that quietly stopped trying something is
one people work around by not using the router.

**`preflight` feeds it.** It used to answer an `Access` per route and nothing read the
answer, so a route that said `Denied` at startup was tried first in every request anyway.
Now `Denied` rests the route for `Breaker::settled`, and `Unknown` does nothing at all: that
variant exists so "could not be checked" never reads as "refused", and acting on one here
would undo the entire reason there are three answers instead of two. `Ready` does nothing
either, because there is nothing to clear at startup and a `Ready` is a claim about a moment
rather than a promise about the next request.

The route is still not dropped. It is rested, which is a thing that ends by itself, so a key
fixed while the program is running is found rather than never tried again. `fell_through`
says "denied at preflight" rather than reporting a failure count of zero, because a count of
zero would send somebody looking for a failure that never happened.

---

### A streamed route is replaceable until the first event, and not after

`Router::stream` follows every rule `Router::chat` follows and adds one that only makes sense
here. Falling through is invisible to a caller who has not seen anything yet. It stops being
invisible the moment a chunk has been handed over: continuing on a second model produces a
sentence neither of them wrote, in one voice, with nothing downstream able to detect it.
Silent corruption of the answer is worse than a failed call, and a failed call is what the
caller gets instead.

The seam is real rather than assumed. `HttpTransport::send_streaming` checks the status
before handing over any bytes, so a 429 or a 503 is an `Err` from `Router::stream` itself and
is fallen through. Anything after that is an `Err` item inside the stream, and
`Transcript::drain` already keeps what arrived.

`Router::stream` returns `(EventStream, Routed<()>)` rather than one value because `Routed`
derives `Debug` and `Clone` and an `EventStream` can do neither. `Routed<T = ChatResponse>`
is generic so that `Routed` still means what it always did.

The two entry points share one body. The parts that would rot if copied are the ones that
matter: a refusal stops everything rather than being asked of the next model, and every
skipped route is reported. Only the method being called differs, so that is the argument.

---

## Hedging is the caller's, and the ledger has to survive it

Racing two providers for the same question and taking whichever answers first is not this
crate's job. Whether doubling the bill is worth the latency, which two to race, how long to
wait before starting the second: those are policy over the caller's own system, the same
reasoning that keeps the engine from deciding what your work needs. A router that
hedged would be one whose cost model its author chose for you.

A caller can already do it, and needs nothing added here. `Provider` is `Send + Sync`, `chat`
takes `&self`, so one `Arc<dyn Provider>` goes to two tasks and `tokio::select!` is the whole
implementation.

**What building it here would have cost.** A hedge needs a policy object (how many, after
what delay, which routes), a way to cancel the losers, and a rule for what `Routed` means
when two routes both ran. That last one is the expensive part: `Routed::route` names the one
route that answered, and `fell_through` means "tried and did not serve". A hedged call fits
neither, so either the type grows a third notion or it starts lying. That is a lot of surface
for something `select!` already does.

**The debt it leaves, which is real.** Drop the losing future and the request was still sent
and will still be billed. No `ChatResponse` came back, so there is no usage, so
`Ledger::record` is never called. The ledger then holds one line, that line was measured, and
`total()` answers `Exact`. A confident, plausible, wrong number, opened by the engine's own
design decision.

`Ledger::record_cancelled` is the fix and it is one line:

```rust
// The losing call. It went, it was billed, its reply was never read.
ledger.record_cancelled(model);
```

`calls()` becomes 2, `unpriced()` becomes 1, the total becomes `AtLeast`. Correct.

It does exactly what `record_unpriced(model, Usage::absent())` does. It exists under its own
name because nobody reading "for a reach with no price list" would think of a cancelled
call, and a method whose documentation does not describe your situation is a method you do
not call.

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

### Shipped tables, and the staleness that comes with them

Anthropic, OpenAI and Gemini each ship a model table and a price book, read off the vendor's
own published pages on the date each row carries. A gateway that wants to know
what something cost gets a number, rather than `None` for every model until they write and
date a table themselves.

**Nothing is invented.** A row is there because a person read a published page on a stated
date and wrote it down. Where a page does not say, the row does not claim: `Entry::new`
starts with every capability off for exactly this reason. Where a fact came from two pages,
the row's `source` says so, and the file says which two.

**A rate that cannot be expressed is left out.** Several models are published in context
bands, one price up to a token threshold and a higher one above. A `Rate` is a flat number
per million and cannot say that. Those models have no row, so they price as unpriced, which
the engine already reports honestly. The tempting alternative is the low band, which is right
until somebody sends a long prompt and then understates every call after that without
anything being able to tell.

**Silent staleness is the failure mode, so a book can be asked.** `PriceBook::age(today)`
gives days since `verified_at`, and `PriceBook::needs_rechecking(today)` applies the rule:
`RECHECK_AFTER_DAYS`, which is 90, and is arbitrary in the way any such number is. What makes
it useful is that it is written down, it is one number, and the crate applies it for you.

`today` is an argument rather than a clock. Every date in a table is already `YYYY-MM-DD`
text, so the comparison is between two things of the same kind, and a test can ask what a
book looks like in 2027 without waiting.

`Recheck` is three variants rather than a boolean, for the same reason `Access` is:

* `Expired` when the book named a date its numbers stop being right and it has passed. Some
  rates are published as introductory with an end date already announced, and a book that
  knows it becomes wrong should say when.
* `Aged` when nobody has checked in longer than the rule allows. A judgement call, and
  reported as one.
* `Undatable` when a date on the book cannot be read. The quiet one: a `verified_at` of
  `"recently"` parses as TOML and would make a book permanently fresh, which is the single
  answer that can never be checked.

Expiry is reported ahead of age when both are true. Ageing says somebody should look; expiry
says the numbers have already changed.

**Why the tables are not behind a `tables` feature.** They were going to be. A feature that
only removes a few kilobytes of static text is a feature nobody sets, and the cost of having
it is worse than that: `from_env` would return a provider with a model table under one
feature set and without one under another, so the same line of code would route differently
depending on how the crate was compiled. Features here exist so a build compiles what it
uses, and this one would have made a build mean something different instead.

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

**And an integer is not money until something says which money.** `Micros` adds whether or
not the two amounts are in the same currency, so `Priced` carries the code from the book that
produced it, `Ledger::total` answers `None` when a run mixes them, and `Ledger::totals` gives
one figure per currency instead.

There is no exchange rate in the engine, and adding one would be the same mistake the rest of
this section avoids: a rate has a date and a source exactly like a price does, and one
invented so that a method could return a single number would produce a figure nobody could
audit. A caller who wants one total across currencies has to say which rate, as of when.

---

## Public structs are `#[non_exhaustive]`, so they have constructors

Fields can be added without a major version, which also means outside code cannot build one
with a literal.

**Every type a provider or transport implementer has to construct therefore has a
constructor.** That gap was found by writing the tests as an outside caller: before the
constructors existed, "write your own provider" did not compile, and nothing inside the crate
would have noticed.

If you add a struct outside code must build, give it a constructor in the same commit.

### What was not marked, and what that cost

The rule was written down and then not applied to everything. `PriceBook`, `Registry`,
`UsageCoverage`, `Total`, `Reach`, `Role`, `Effort`, `Thinking` and `Method` were all
exhaustively constructible or exhaustively matchable from outside, and 0.2.0 paid for it:
`cargo-semver-checks` failed three lints across two pull requests, and the fix was a version
bump rather than a change to the code.

| Type | What broke |
|---|---|
| `PriceBook` | gained `expires_on` |
| `UsageCoverage` | gained `Estimated`, which also moved two discriminants and changed the derived `PartialOrd` |
| `Total` | gained `About` |

All nine are marked now. Doing it in 0.2.0 cost nothing, because 0.2.0 was already a breaking
release; every release after this one it would have cost a major bump of its own.

**Two kinds of type are deliberately left alone.**

`Micros` is a newtype whose whole purpose is transparent construction. `Micros(5)` is the
API, and taking it away would buy nothing: a newtype cannot grow a second field without
becoming a different type anyway.

A struct whose fields are all private is already unbuildable from outside, so marking it adds
nothing. `Budget` is the example: its three fields are private and it has `Budget::of` and
accessors.

**Enums are the half that is easy to forget.** `#[non_exhaustive]` on an enum is not about
construction, it is about `match`: outside code must carry a `_` arm, which is what lets a
variant be added later. Inside the engine the attribute does nothing, which is why
`Breaker::opening_for` can still match `Error` exhaustively and refuse to compile when a
variant appears. That is the pattern to copy, not to work around.

---

## The contract suite is applied to the crate's own providers

`testkit::assert_provider_contract` is behind a feature for outside users, and
`tests/every_provider_honours_the_contract.rs` runs it against all three of ours.

A suite only outsiders have to pass is a suite nobody inside is held to. It has already earned
this: applying it caught a provider claiming to know every model name, which turns a typo
into a real model.

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
| `anthropic`, `openai` | 31 | Both protocols. You supply the transport |
| `+ reqwest` | 105 | And a bundled client, with `from_env` |

The first two are on by default and `reqwest` is not, so a build that reaches nothing
compiles no network stack. `embeddings`, `image-generation` and `audio` are off by default,
each a trait of its own, so a build that only chats compiles none of them. The gateway's
`server` feature turns on what it needs, and CI builds every feature alone so one that only
compiles beside another is caught.

**Count distinct crates, not lines of `cargo tree`.** These read 52 and 250 for a while.
Those were `cargo tree | wc -l`, which prints a crate once per dependent that reaches it, so
every figure was roughly 1.8x the truth. The argument held and the numbers did not, which is
the more embarrassing half. `cargo tree --prefix none | sort -u` is what these are now.

Examples are gated with Cargo's `required-features` rather than a `cfg` attribute inside the
file, so `cargo run --example` reports the missing feature instead of building a binary with
nothing in it.

---

## A fixture cannot check a field name that was wrong from the start

Every fixture in this repository was written here. That makes them good at one thing and
blind to another.

They catch a **regression**: a translation that used to produce this and now produces that.
They cannot catch a **mistake**, because a field name read wrong from a vendor's
documentation is read the same wrong way into the fixture, and the two agree forever. Three
hundred passing tests say nothing about it, and nothing inside the crate can.

`tests/against_a_real_endpoint.rs` is the only thing that can. Every test in it is
`#[ignore]`, so `cargo test` never runs one and CI never spends money, and a test whose key is
missing skips itself rather than failing, so one key is enough to run the file.

**What it asserts is not "it answered".** A fixture already proves the crate can read a reply
it was handed. These are the four claims only a real endpoint settles:

* **Usage is `Exact`, not `Partial`.** A `Partial` means a field the engine reads by name was
  not there under that name. That is precisely the shape of the mistake, and every cost report
  built on it is a floor nobody knows is a floor.
* **The reply names a real model**, and it is printed, because what a vendor actually serves
  for a given alias is a fact worth having in a commit.
* **The stop reason mapped to something** rather than to a fallback. A provider that maps
  every reason it does not recognise onto `EndTurn` reports a truncated answer as a complete
  one.
* **A streamed call and a whole one agree**, against the wire rather than against two
  fixtures written the same afternoon.

Gemini is exempted from the first of those and says why in the test: that API reports no cache
write count at all, so its usage is `Partial` by design, and asserting `Exact` would be
asserting that a documented decision is a bug.

`LLMR_RECORD` writes what came back to a directory, because a reply that stayed in somebody's
terminal is a call they made and a reply committed here is a call anybody can check. The
`Against a real endpoint` workflow does the same on a runner, dispatched by hand behind a
gated environment, never on a push.

---

## The client API speaks the OpenAI shape, and refuses what it cannot carry

**Why the OpenAI shape and not the engine's own.** Every SDK, framework and editor already
speaks it, so adopting llmr is a base URL change. The cost is that the shape has no place for
a thinking signature, a cache breakpoint or an opaque block, so those do not cross it. An
Anthropic Messages endpoint beside it is the way to carry them, and is listed as a gap rather
than approximated.

**A field that cannot be honoured is a 400, not ignored.** `n = 3`, stop sequences, a
forced `tool_choice`, `json_object`: each would be sent without the thing asked for and
billed anyway. That is the engine's `Needs::unmet_by` rule applied at the edge. Fields that
cannot change what a client is owed (`user`, `seed`, `parallel_tool_calls`) pass.

**A refusal is a 200 with `content_filter`**, which is how the shape writes one. As an
error status it would send every client's retry logic asking the same question again, the
thing the router's refusal rule exists to stop.

**A provider rejecting llmr's credential is a 502, not a 401.** The client's token was fine; a
401 would send somebody checking the wrong credential.

**A model that is switched off says so.** `model_not_enabled` rather than `model_not_found`,
because the fix is different: one is a typo in the client, the other is a switch in the panel.

---

## Configuration is data, managed over REST

llmr is run by a panel, not edited by hand, so everything it serves is set through the
management API and kept in SQLite on the container's volume. There is no configuration file.

**Why a database and not a file the panel writes.** A file needs somewhere to be written from,
a restart or a reload signal to take effect, and a format two programs agree on. The API
validates a change before it is stored, answers with what llmr made of it (usable routes,
unavailable ones and why), and takes effect on the next request. SQLite because the state is
small, lives with the container, and needs no second service.

**Credentials are sealed with a key the database never holds.** `LLMR_MASTER_KEY` comes from
the environment; each credential is sealed with XChaCha20-Poly1305 and a random 192-bit nonce,
long enough that choosing it at random is safe with no counter to keep. A copy of the volume
alone opens nothing. The API is write only for credentials: in on `POST` and `PATCH`, out as
four characters. A sealed marker in the database makes a wrong key fail at startup, with a
message, instead of producing a service whose every provider fails on first use.

**No users, no roles.** A panel is the only client of the management API, and it has its own
users. Roles here would be a second permission system to keep in step with the first. The
boundary is the port: a private network, or `LLMR_TOKEN`, or both. What that costs is written
in `SECURITY.md` rather than left to be discovered.

**A change rebuilds the whole gateway and swaps it in.** Providers and routers are immutable,
which is what lets the request path hold no lock. So a change reads the store, builds a new
gateway, and replaces the old one behind an `Arc`; requests in flight finish on the one they
started on. The price is that routers' failure counts start again after every change, which
a management API that changes rarely can afford, and a lock on every request could not.

**One bad row does not take the rest down.** A provider that cannot be built is left out and
named in `/manage/status`; a route that cannot be used is left out of its set and named in
that set's `unavailable`, with the reason. Refusing to start, the right answer for a file read
once at boot, would be the wrong one for a database a panel edits while traffic flows.

**A model needs capabilities to be enabled.** Routing reads what a model can do, and a model
nothing is known about would be a route no request can ever select, silently. So enabling one
the release's tables do not list is refused until the panel says what it can do.

**A test is free unless asked otherwise.** Testing a provider asks for its model list, which
proves the credential and the entitlement without generating a token. The live test that sends
one real request is opt in, because a check that spends money is one that gets skipped.

**Route names use the provider ids the panel chose.** The engine's providers name themselves
after their protocol, so two Anthropic accounts would both log as `anthropic`. A thin wrapper
puts the stored id in front and delegates everything else.

**Direct `provider/model` routers are cached only for enabled models**, so their breakers
remember between requests and a client cannot grow the cache by inventing names.

---

## A model has one kind, and a route set serves one

Chat, embeddings, image generation, speech and transcription are five endpoints, and a model
answers at one of them. The management API records which as the model's `kind`.

**On the model row, not on the provider.** OpenAI serves all five from one key, and so can an
OpenAI-compatible server. What decides the endpoint is the model: a chat model asked for
vectors is a request that cannot be sent at all. The provider type only bounds it, with the
`kinds` it has endpoints for, so an Anthropic model cannot be enabled as an embedder.

**A set serves the kind of its first enabled route.** A chat request has nothing to say to an
embedding model, and a set that mixed them would fall through from one to the other on every
call. A route of another kind is listed in `unavailable` with the reason, the same treatment
as any other route that cannot serve, rather than refusing the whole set.

**The wrong endpoint is named, not reported missing.** An embedding set sent to
`/v1/chat/completions` is a `400 wrong_endpoint` saying where it belongs. `model_not_found`
would send somebody looking for a typo that is not there, the same reason a switched off model
says so.

**Capabilities describe chat, and the other kinds take none.** Routing on capabilities exists
to skip a model that would half answer a request. An embedding or a speech call has nothing
of that shape to match on, so a model of another kind needs no capabilities to be enabled,
sending some is a `400`, and a model moved off chat drops the ones it had: what was said
about a chat model says nothing about the same id used another way.

**A kind this build does not know reads as chat.** A row written by a newer llmr keeps
working as the one kind every provider serves, rather than failing the whole snapshot.

### A request that is not chat goes through a simpler loop

The engine's `Router` routes a `ChatRequest` over `Provider`, which chats. Embedders, image
generators, speech synthesizers and transcribers are traits of their own, for the reason
embeddings are, so the gateway tries their routes with a loop of its own in `gateway.rs`. It
keeps the rules that change what a caller is owed:

- each route gets the set's `retry_attempts`, and only a failure worth repeating is repeated;
- a refusal stops, and is not shopped to the next model;
- the `on_device` floor holds, from the set or from the header;
- `deadline_secs` bounds the whole request, and a wait that would cross it is not started.

It leaves out ordering and the breaker. Routes go in the order listed, and a failing route is
tried again on the next request. `cheapest` would compare nothing here: the shipped price
books list chat models only, so every vendor media model is unpriced. Without the breaker,
a route that is down costs one failed call per request until it recovers; that is the
price of the simpler loop. `order` and `breaker` are stored on such a set and ignored,
and the documentation says so rather than letting a panel believe otherwise.

**What a provider cannot honour is refused before the call.** Gemini draws one picture per
call at an aspect ratio, and speaks WAV or raw samples at one speed. A request for anything
else fails that route with nothing sent, because a picture of the wrong shape or a recording
in the wrong format is billed and then useless. The next route is tried, as for any failure
that is not a refusal.

**A refusal is a `422`.** Chat writes one as a `200` with `content_filter` because its shape
has a place for it. An embedding list, a picture list and a recording have none, and an
empty one would read as an answer.

---

## Usage is recorded beside a request, never in its way

Every request the client API handles leaves one row: what was asked for, the route, tokens,
cost, latency, outcome. A panel needs the totals, and the engine already reports usage
honestly; what was missing was somewhere to keep it.

**Written in the background.** A row goes into a bounded queue and one task writes rows in
batches of up to 500 per transaction. A request never waits on the disk. When the queue is
full the row is dropped and the log says so: a missing usage row is a smaller failure than
traffic stalling behind a slow volume. On shutdown the queue is flushed, with a time limit so
a stuck disk cannot hold a container up.

**Never content.** The row has no field a prompt or a reply could go into, the same shape
rule the engine's spans follow.

**Refused requests are recorded too.** A `model_not_found` or `model_not_enabled` never
reaches a provider, and it is still worth seeing: it is a client asking for something the
panel switched off.

**Four answers for cost, and none of them is zero-for-unknown.** `priced` is the provider's
published rate times the usage it reported; `partial` is the same with some usage fields
missing, so a floor; `unpriced` is a paid provider with no known rate or no reported usage;
`free` is a self hosted model. A total adds amounts only within a currency, and its
`cost_complete` is false the moment any request was `partial` or `unpriced`. Presenting a
floor as the bill is the mistake the engine's `Total` exists to prevent, and a panel is where
somebody would make it.

**Priced by the served model, then the routed one.** Vendors answer under dated aliases a
price book does not list. Trying the served name first keeps a book that does list it exact;
falling back to the route's model keeps a dated alias from turning a priced call unpriced.

**A cost uses the gateway the request started on.** A provider changed or removed while a
stream runs does not reprice it against something it never used.

---

## Naming and prose

Tests are named after the claim they make, not the function they call.
`a_reply_with_no_readable_content_is_an_error_not_an_empty_answer` tells a reader what breaks
if it fails; `test_chat` does not.

Comments say why, not what. If a decision would look wrong to somebody reading it cold, the
comment says what would break without it.

No em dashes anywhere, in code or prose.
