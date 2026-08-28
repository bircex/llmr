# Security

## Reporting

Report a vulnerability through GitHub's private advisory form on this repository, under the
Security tab. Please do not open a public issue for something exploitable.

Tell us what you found, how to reproduce it, and what an attacker gets. We will confirm we
have it, and we will tell you when a fix is released.

## What this crate handles

An API key, and whatever you put in a prompt. Both are worth thinking about.

**Keys.** `Secret` masks in `Debug` and `Display`, does not implement `Serialize`, and
overwrites its buffer on drop. That makes the common accidents into unreadable output and
compile errors rather than a key in a log file. It does not defeat a memory dump taken while
the process is running.

Reading a key is `expose()` or `expose_str()`, named that way so a review and a search both
find every place it happens.

**Prompts.** Where they go is what `Reach` is for, and getting it wrong is the likeliest
security problem in a program using this crate:

```rust
use llmr::Reach;

assert!(Reach::LocalCli.uses_local_credential());
assert!(!Reach::LocalCli.is_on_device());
```

A vendor command line tool signs in on your machine and still sends every prompt to the
vendor. If your code treats "the credential is local" as "the data stays here", it will send
something private to a third party and record it as safe. Only `Reach::SelfHosted` keeps the
data.

The reach of an OpenAI compatible endpoint is given by you, not guessed. A model on your
laptop and a hosted API answer the same request shape, and this crate cannot tell them apart.
Setting it wrong is silent.

## What this crate does not do

It does not validate model output. Anything a model returns is text somebody else produced,
including any tool call arguments in it. Treat it as data, never as instruction, and never
pass it to a shell, a query, or a file path without checking it yourself.

It does not retry for you. `Error::is_retryable` is a hint that the failure was not your
fault and not permanent. It does not say the call is safe to repeat, which is a question
about your request. A timeout is retryable and may still leave you paying for two answers.

## Supported versions

The latest published release. This crate is pre 1.0, so fixes go into a new minor version
rather than being backported.
