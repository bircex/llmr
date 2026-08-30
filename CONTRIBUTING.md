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
cargo fmt --all -- --check
cargo clippy --all-features --all-targets -- -D warnings
cargo clippy --no-default-features --all-targets -- -D warnings
cargo clippy --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --all-features --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo test --all-features
cargo test
```

All eight must be clean, and warnings count. Three of them look redundant and are not: a
clippy lint can fire under one feature set and not another, a doc link to a feature gated
item resolves under `--all-features` and nowhere else, and so does a *doctest* naming one.
The last line is there because two README examples had been failing on the default feature
set for as long as this list ended one line earlier.

Run them on the toolchain in `rust-toolchain.toml` rather than whatever your machine has.
That file exists because these commands once passed on a laptop running 1.97 and failed on
a runner running 1.98 for weeks, with the crate unchanged. `rustup` picks it up on its own
if you are in the repository.

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
    api/         the shared machinery for reaching over the network: ApiProvider + Protocol
    cli/         the shared machinery for a subprocess: LocalCli + Envelope
    anthropic/   api.rs the Messages protocol, cli.rs the Claude Code preset
    openai/      api.rs the chat completions shape, cli.rs the Codex preset
  model.rs       Reach, ModelId, ModelCapabilities
  registry.rs    what a provider serves and what it can do
  provider.rs    the one trait
  router.rs      which provider a request goes to
  transport.rs   the HTTP boundary, and a reqwest implementation of it
  error.rs secret.rs testkit.rs
```

Two groupings, doing two jobs. **What is shared follows the reach**, because reach is what
decides how a model is spoken to: everything an API provider does apart from writing JSON is
identical, and so is everything a subprocess does apart from its arguments. **What is chosen
follows the vendor**, because that is what a caller picks, and the same models turn up behind
more than one reach.

So `anthropic/api.rs` and `anthropic/cli.rs` are short. The engine is not in them.

Everything else stays flat. A directory holding one file is a directory that exists to look
organised.

## What CI will run

The eight above, plus three you would not usually run by hand:

- **`cargo deny check`** — licences against an allowlist, advisories denied, sources limited
  to crates.io, duplicate versions warned. `deny.toml` says why each allowed licence is
  there. Install it with `cargo install cargo-deny --locked` if you want to run it locally.
- **`cargo publish --dry-run`** — what would actually ship. `exclude` in `Cargo.toml` keeps
  CI configuration, the roadmap, the design notes and `deny.toml` out of the package.
- **`cargo-semver-checks`** — skipped, with a note saying so, until 0.1.0 is published;
  there is no released API to compare against. After that it is the job that catches a break
  nobody meant: everything public here is
  `#[non_exhaustive]`, which is exactly the arrangement where somebody assumes every change
  is additive. Adding a required method to `Provider` is breaking. Narrowing a return type
  is breaking. Neither looks like it in a diff.

## Writing a provider

You are almost certainly writing a **protocol** or a **preset**, not a client.

For something over the network, implement `providers::api::Protocol`: what URL, what headers,
what JSON goes out, what comes back. The transport, the credential, the status codes and the
error mapping are `ApiProvider`'s. There is nowhere to hold state, and that is on purpose.

For a command line tool, write a preset on `providers::cli::LocalCli`: a program name, its
arguments, and an `Envelope` saying where in its output the answer and the usage are. The
spawning, the deadline, the kill on drop and the difference between a missing binary and a
silent one are `LocalCli`'s.

Either one goes in a file under **whoever you reach and whoever the credential pays** —
`providers::<who>::api` or `providers::<who>::cli` — beside whatever other reaches that node
already has. For a first party API that is the vendor. For a gateway serving several vendors'
models over one credential, such as Bedrock, it is the gateway: `providers::bedrock::api`,
not `providers::anthropic::bedrock`. Claude through Bedrock is not Anthropic answering, and
the import line should not suggest it is.

A new node is a new directory with a `mod.rs` saying which reaches it has and what each one
can carry, since a caller comparing two of them is the reason the directory exists. If it is
a gateway, add a line to each vendor it serves pointing at it — somebody looking for Claude
on Bedrock will look under `anthropic` first, and that pointer is where they are already
looking.

`docs/DESIGN.md` has the reasoning, including the option that was rejected and why.

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
