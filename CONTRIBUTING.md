# Contributing

Thanks for looking. This is a small crate with a narrow job, and the rules below exist to
keep it that way.

## Read this first

[docs/DESIGN.md](docs/DESIGN.md) is the reasoning behind the decisions in this crate. A good
number of them look like something to tidy away until you know what breaks without them, and
the tidying is the failure mode this crate is most exposed to.

[ROADMAP.md](ROADMAP.md) is what is left before 0.1 and what each phase needs.

## What belongs here

One question: how do I reach this model, and what did it cost.

Adding a provider, fixing a translation, correcting a model table or a price: yes.

A tool loop, memory, orchestration, or choosing a model for a task: no. Those are decisions
about your system, and a library that made them would be one you had to fight.

## Before you open a pull request

```sh
cargo fmt --all
cargo clippy --all-features --all-targets
cargo test --all-features
cargo doc --all-features --no-deps
```

All four must be clean. Warnings count.

## The rules the code is held to

**No panicking in the library.** `unwrap`, `expect`, `panic!`, `todo!` and `unimplemented!`
are denied. A library that decides to stop the process has taken a decision that belonged to
the program using it. Tests may panic; that is their job.

**No lock held across an await.** `await_holding_lock` and `await_holding_refcell_ref` are
denied. A provider holds nothing mutable, so a call needs no lock at all. If you find
yourself wanting one, that is worth discussing in an issue first.

**No unsafe.** Forbidden, not denied.

**Every public item is documented.** `missing_docs` is denied. Say what it is for, not what
it is. "Returns the model id" is not documentation.

## Where things live

```
src/
  chat/          what a call is made of: message, request, response
  cost/          what it consumed and what that is worth: usage, pricing
  providers/
    api/         over the network. ApiProvider + Protocol, then one file per shape
    cli/         a local tool as a subprocess. LocalCli + Envelope, then one file per tool
  model.rs       Reach, ModelId, ModelCapabilities
  registry.rs    what a provider serves and what it can do
  provider.rs    the one trait
  router.rs      which provider a request goes to
  transport.rs   the HTTP boundary, and a reqwest implementation of it
  error.rs secret.rs testkit.rs
```

Grouped by how a model is reached, because that is the difference that matters: what a
provider can carry, what it reports, and whose credential pays all follow from it.

Everything else stays flat. A directory holding one file is a directory that exists to look
organised.

## Writing a provider

You are almost certainly writing a **protocol** or a **preset**, not a client.

For something over the network, implement `providers::api::Protocol`: what URL, what headers,
what JSON goes out, what comes back. The transport, the credential, the status codes and the
error mapping are `ApiProvider`'s. There is nowhere to hold state, and that is on purpose.

For a command line tool, write a preset beside `providers::cli`: a program name, its
arguments, and an `Envelope` saying where in its output the answer and the usage are. The
spawning, the deadline, the kill on drop and the difference between a missing binary and a
silent one are `LocalCli`'s.

Then run the contract suite against it:

```rust,no_run
# #[cfg(feature = "testkit")]
# async fn example(mine: &impl llmr::Provider) {
llmr::testkit::assert_provider_contract(mine, "a-model-you-serve").await;
# }
```

Three things the suite is checking, and they are the ones that are easy to get wrong:

1. `capabilities` returns `None` for a model you do not know, and a capability set with
   everything off for a model you know and cannot do much with. Those are different answers
   to different questions.
2. A reply you could not read is an error. Never an empty message. A caller cannot tell an
   empty answer from a failure, and one of them means carry on.
3. Usage the provider did not report is `Usage::absent()`, not zeros. An unknown cost
   written as zero becomes a free call in every report that adds it up.

Put your provider behind a feature and add it to the table in the README.

## Model tables and prices

Every row carries a `source` and a `verified_at`. A row without them is refused at parse
time, and that is deliberate: the first time one number turns out to be wrong, there is no
way to tell which of the others still hold.

If you update a table, update the date, and say in the pull request where you checked.

## Tests

Name a test after the claim it makes, not after the function it calls.
`a_reply_with_no_readable_content_is_an_error_not_an_empty_answer` tells a reader what
breaks if it fails. `test_chat` does not.

If a test needs a comment to explain why the claim matters, write the comment. The next
person to see it will be deciding whether to delete it.

## Commits and versions

The crate follows semantic versioning. Before 1.0, a breaking change is a minor bump.

Adding a field to a struct marked `#[non_exhaustive]` is not breaking. Removing one is.
Most public structs are marked that way for exactly this reason, which also means callers
build them through constructors rather than literals. If you add a struct that outside code
must be able to build, give it a constructor in the same commit.
