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

## The module tree groups by who you reach; the shared machinery groups by reach

`providers::anthropic::{api, cli}` and `providers::openai::{api, cli}` are what a caller
imports. `providers::api` and `providers::cli` are what a contributor builds on.

This was the other way round once — `api::anthropic` beside `cli::claude` — on the reasoning
that reach is the difference that matters. Reach *is* the difference that matters, and that
turned out to be an argument for something else.

**What is shared follows the reach.** Everything an API provider does apart from writing JSON
is identical, and so is everything a subprocess does apart from its arguments. That is why
`ApiProvider` and `LocalCli` exist and why they sit under `api/` and `cli/`. Reach is the
axis the *code* is organised by, and it still is.

**What is chosen follows the vendor.** A caller knows which vendor before they know which
reach, and the same models turn up behind more than one. Anthropic's answer over the Messages
API and through Claude Code, and those differ in what they can carry rather than in what they
are. Reach-first put that comparison two directories apart, and named the halves
inconsistently while it was at it: `api::anthropic` for the company, `cli::claude` for the
product. A caller weighing one against the other could not see there was a choice.

**If this became reach-first again**, the vendor files would have to move but nothing would
break, because the engines are not in them — `anthropic/cli.rs` is forty lines. The cost is
paid by the reader, not the compiler, which is exactly the kind of cost that goes unnoticed
until somebody sends a prompt through the tool because they never saw the API beside it.

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
module cannot answer the question and asks instead. And it is why `anthropic::cli` sits
under Anthropic despite being a subprocess: the credential is Claude Code's login, and the
prompt still goes to Anthropic.

**Option 2 is the one to argue with, because it is the friendly one.** A caller looking for
Claude on Bedrock will look under `anthropic` first, and option 2 is where they would find
it. It is rejected because it makes `anthropic::api` and `anthropic::bedrock` read as two
routes to the same place, and they are not: different endpoint, different credential,
different company holding your prompt. This crate exists to keep that distinction legible,
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
would simply be wrong, in the direction this crate exists to catch, and no test could see
it.

---

## Embeddings are a trait here, not a second crate

Embeddings are a different question from chat: different request, different reply, different
usage shape, no messages, no stop reason, no reasoning, no tools. Almost nothing in `chat/`
applies (#26).

**They belong in this crate, as their own trait, behind a feature.** Not as a method on
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
rather than an error**, and this crate now has two of them written down.

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
like every other capability. A command line tool that prints one JSON document when it
finishes cannot stream whatever model is behind it, and it says so rather than failing.

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
wait. Waits this crate computes itself are jittered, because two callers that failed together
coming back together is how a provider recovering from a fault gets knocked over again.

**Jitter without `rand`.** A dependency on `rand` to spread retries apart would cost more
crates than the whole OpenAI protocol. Nothing here is a secret, so the clock's nanoseconds
through an xorshift are enough. If this ever needs to be unguessable rather than merely
uneven, that is a different requirement and it should arrive with its own reason.

**If retrying became the default**, the first timeout in somebody's production run would
double a bill, and the line that did it would not appear in any diff.

---

## Spans carry facts, and structurally cannot carry content

Behind the `tracing` feature, off by default, because a library that emits whether you asked
or not is one people work around. With it off there is no dependency and no work.

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

## Four crates are part of the public API, and a major bump of any is a breaking change

Found by reading the surface rather than the manifest (#19). These types appear in signatures
callers write, so they are promises even though nothing says so at the call site:

| Crate | Where it shows | What a major bump costs |
|---|---|---|
| `serde_json` | `Value` in `ToolSchema::parameters`, `ContentBlock::ToolUse::input` and `Opaque::raw`, and across `Protocol` | A caller's `Value` stops being this crate's `Value` |
| `serde` | `Serialize`/`Deserialize` on most public types | The same, for anything that round trips a request |
| `futures-core` | `Stream` inside `EventStream` | Every `stream` implementation |
| `reqwest` | `Client` in `Reqwest::with_client`, behind the `reqwest` feature | Only callers who build their own client |

Three of the four are unavoidable and worth it: a JSON value has to be a JSON value somebody
else can build, and a stream has to be a `Stream` other code can consume. Hiding them behind
newtypes would mean converting at every boundary and would not remove the coupling, only
disguise it.

**What this means in practice** is that a `serde_json` 2.0 is a minor bump of this crate
before 1.0 and a major one after, and that is a decision somebody should make deliberately
rather than discover from a bug report. It is written down here because nothing in the
manifest distinguishes a dependency that is an implementation detail from one that is part
of the promise.

---

## How the public surface is counted

The roadmap said 180 in one place and 189 in another, and neither said what it counted. Both
were wrong in the way that matters: an unmethodical number cannot be compared to a later one,
so it cannot tell you the surface grew.

The method is now stated, and it is a command:

```sh
cargo +nightly public-api --all-features
```

That prints every public item including the trait implementations `derive` writes, which is
the honest total and is dominated by them. What a reader of the docs meets is smaller, and
the useful figure is whichever one you pick — as long as the next person picks the same one.
The roadmap records both and the command that produced them.

After 0.1.0 the same tool answers a better question than "how many": `cargo public-api
--diff` against the published version says what *changed*, which is what
`cargo-semver-checks` is in CI to enforce.

---

## Signing is a transport concern, not a protocol one

Bedrock authenticates with SigV4 rather than a bearer token, and #22 asked where that
belongs. It belongs in [`HttpTransport`], and this crate ships no implementation of it.

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
nobody could test against the real thing from inside this crate. `aws-sigv4` exists, most
programs reaching Bedrock already have `aws-config` for credentials, and wrapping a transport
is ten lines. The crate already makes this bargain for HTTP itself: `reqwest` is a feature,
not a requirement.

**If signing moved into `Protocol`**, every protocol would gain a method that all but one
ignores, `ApiProvider` would have to hand it the request it is about to send, and the purity
that makes protocols shareable would go. The one gain would be that Bedrock needed no wrapper,
which is the smallest of the three things at stake.

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

**And an integer is not money until something says which money.** `Micros` adds whether or
not the two amounts are in the same currency, so `Priced` carries the code from the book that
produced it, `Ledger::total` answers `None` when a run mixes them, and `Ledger::totals` gives
one figure per currency instead.

There is no exchange rate in this crate, and adding one would be the same mistake the rest of
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
| `anthropic`, `openai` | 31 | Both protocols. You supply the transport |
| `+ reqwest` | 105 | And a bundled client, with `from_env` |
| `cli` alone | 30 | A local tool as a subprocess, no network code |

The first two are on by default and `reqwest` is not. Almost every program already has an HTTP
client, and adding this crate should not add a hundred more.

**Count distinct crates, not lines of `cargo tree`.** These read 52, 250 and 53 for a while.
Those were `cargo tree | wc -l`, which prints a crate once per dependent that reaches it, so
every figure was roughly 1.8x the truth. The argument held and the numbers did not, which is
the more embarrassing half. `cargo tree --prefix none | sort -u` is what these are now.

Examples are gated with Cargo's `required-features` rather than a `cfg` attribute inside the
file, so `cargo run --example` reports the missing feature instead of building a binary with
nothing in it.

---

## A command line preset is four claims about somebody else's program

What to run, where the answer is in the JSON, what the usage fields are called, and what the
probe proves. `LocalCli` does the spawning, the deadline, the kill on drop, the prompt
assembly and the envelope reading, so a preset is a small file. Three of the four things in
it cannot be checked from here.

**A fixture written to match a preset proves the preset matches itself.** So the envelopes in
`tests/recorded/` came off a real tool, and `ProcessRunner` replays one: everything about the
provider stays real except the program, which is the one part that cannot be in a repository.
That is also what finally puts the presets through the contract suite, which they had never
been in, because without a runner they could not run at all.

**Guessing the usage names is not an option.** Whether a tool's prompt count is the whole
prompt or the uncached remainder decides whether a number is right or merely looks right, and
`Usage::input_tokens` means the remainder. The recorded Claude Code run settles it: 4,685
input beside 20,208 written to cache is a remainder, and a total would have read 24,893 and
looked entirely reasonable.

**The recording also found a bug the documentation had talked us out of looking for.** The
preset reported `StopReason::Other` for every reply, with a comment saying a command line tool
does not say why it stopped. This one does. `Other` is not `is_complete`, so every caller
asking whether an answer had finished was told "no", forever. `Envelope::with_stop_reason`
reads it where a recording shows one, and a tool that says nothing is still `Other`, because
a truncated reply must not look finished.

**A preset with no recording is a preset whose field names nobody has checked**, and this
repository says which those are rather than implying they are all equal.

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

* **Usage is `Exact`, not `Partial`.** A `Partial` means a field this crate reads by name was
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

## Naming and prose

Tests are named after the claim they make, not the function they call.
`a_reply_with_no_readable_content_is_an_error_not_an_empty_answer` tells a reader what breaks
if it fails; `test_chat` does not.

Comments say why, not what. If a decision would look wrong to somebody reading it cold, the
comment says what would break without it.

No em dashes anywhere, in code or prose.
