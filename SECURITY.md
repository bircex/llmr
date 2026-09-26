# Security

## Reporting

Report a vulnerability through GitHub's private advisory form on this repository, under the
Security tab. Please do not open a public issue for something exploitable.

Tell us what you found, how to reproduce it, and what an attacker gets. We will confirm we
have it, and we will tell you when a fix is released.

## What the gateway holds

Two kinds of key, and whatever your projects put in a prompt.

**Provider keys** are read from environment variables at startup and never from the
configuration file, so `llmr.toml` can be committed and mounted read only. Inside the process
they are held as `Secret`, which masks in `Debug` and `Display`, does not implement
`Serialize`, and overwrites its buffer on drop. That turns the common accidents into
unreadable output and compile errors rather than a key in a log line. It does not defeat a
memory dump taken while the process runs.

**Client keys** (`LLMR_API_KEYS`) are what your projects present. They are compared in time
that does not depend on where they differ. The gateway refuses to start with none unless
`auth = "none"` is set, and anybody who can reach the port of a gateway with `auth = "none"`
can spend every configured provider's money.

**Prompts and replies are never logged.** A request is logged as the name asked for, the
route that answered, attempts, token counts and the stop reason, and the logging code has no
path to a message body.

## Where prompts go

That is what `reach` in the configuration and `on_device` on a model are for, and getting it
wrong is the likeliest security problem in a deployment.

A name marked `on_device = true` is only ever served by a `self-hosted` route, even when every
local route is down; a fallback does not relax it, and a client header can tighten it but not
loosen it. The reach of an `openai-compatible` provider is whatever you write: a model on
your own hardware and a hosted API answer the same request shape, and the gateway cannot tell
them apart. Setting it wrong is silent.

## What the gateway does not do

**It does not terminate TLS.** It serves plain HTTP. Keep it on a private network, or put a
reverse proxy with TLS in front of it before exposing it beyond the host: client keys travel
in a header. [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) has an example.

**It does not validate model output.** Anything a model returns is text somebody else
produced, tool call arguments included. Treat it as data, never as instruction.

**It does not rate limit or cap spending per client yet.** Every valid key can use every
configured name. Hand out keys accordingly.

**Retries are configured, not free.** A route retries a rate limit or a transient failure up
to `retry_attempts` times. A timeout is not retried, because the provider may have finished
the work and billed for it, and asking again would pay for a second answer.

## Supported versions

The latest release: the newest `v*` tag, its image and its binaries. The project is pre 1.0,
so fixes go into a new release rather than being backported.
