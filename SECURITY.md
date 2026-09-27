# Security

## Reporting

Report a vulnerability through GitHub's private advisory form on this repository, under the
Security tab. Please do not open a public issue for something exploitable.

Tell us what you found, how to reproduce it, and what an attacker gets. We will confirm we
have it, and we will tell you when a fix is released.

## What llmr holds

**Provider credentials**, entered through the management API. Each one is sealed with
XChaCha20-Poly1305 under `LLMR_MASTER_KEY`, with a fresh random nonce, before it is written
to the database. The key lives only in the process environment, never on the volume, so a
copy of the database alone opens nothing. The API never returns a credential: it shows the
last four characters. Inside the process a credential is held as `Secret`, which masks in
`Debug` and `Display`, does not implement `Serialize`, and overwrites its buffer on drop. None
of this defeats a memory dump of the running process.

The database carries a sealed marker, so starting with a different master key fails at once
and says so, instead of starting with every provider broken.

**The rest of the configuration**, in the clear: provider ids, base URLs, which models are
enabled, route sets. Nothing in it is a secret, and a panel needs to read all of it.

**Prompts and replies are never stored and never logged.** A request is logged, and recorded
in the usage table, as the name asked for, the route that answered, attempts, token counts,
cost, latency and the outcome. That table has no column a prompt or a reply could go into.

## Who can change it

There are no users or roles. Whoever can reach the port can call models, and can use the
management API to add providers, replace credentials and point traffic anywhere. So the port
is the boundary, and there are two ways to hold it:

- **A private network** that only your panel and your apps are on. The default compose file
  publishes the port on loopback only for this reason.
- **`LLMR_TOKEN`**, one or more tokens every call must present. Compared in time that does not
  depend on where they differ.

With `LLMR_TOKEN` unset, llmr says so in its log at every start.

## Where prompts go

That is what a provider's `reach` and a route set's `on_device` are for, and getting it wrong
is the likeliest security problem in a deployment.

A route set marked `on_device` is only ever served by a `self-hosted` route, even when every
local route is down; a fallback does not relax it, and a client header can tighten it but not
loosen it. The reach of an `openai-compatible` provider is whatever the panel writes: a model
on your own hardware and a hosted API answer the same request, and llmr cannot tell them
apart. Setting it wrong is silent.

A command line provider is always `local-cli`: the tool runs in the container and sends the
prompt to its vendor, so it never serves an `on_device` route set.

## Command line tools

Claude Code, Codex and Gemini CLI run inside the container, as llmr's own user, one process
per request. They are other people's programs that can act on a machine if allowed to, so
each call is held in:

- **No tools.** Each is started with every tool it has switched off: no reading files, no
  running commands, no web search, no sub agents. A prompt that asks for any of it gets a
  model that has nothing to call.
- **Nothing inherited.** The environment is emptied and rebuilt: `PATH`, a home of its own,
  the proxy and certificate variables, and the one key it is calling with. It never sees
  `LLMR_MASTER_KEY`, `LLMR_TOKEN` or another provider's key.
- **Nothing kept.** Each call runs in a new directory under `/var/lib/llmr/run`, which is its
  home, working directory and temporary directory, and which is removed when the call ends.
- **Nothing left running.** The tool runs in a process group of its own, killed as a whole
  when the call ends, times out or is abandoned.
- **Nothing to read in llmr.** A process can read the environment and memory of any other
  process of the same user through `/proc`. llmr marks itself undumpable, which makes those
  files root's. That is also why the image has no tini: an init started with llmr's
  environment would hold the master key in a process that cannot be marked, so llmr is its
  own init. Do not run the image with `--init`.

**Subscription sign ins** are sealed like any credential. A call gets its own copy in its
directory, and a sign in the tool refreshed is read back and sealed into the database before
the directory is removed; the plain file never outlives the call.

What is not contained: the prompt and the key go to the vendor, which is what the tool is
for, and a tool can still read what any process of llmr's user can read in the container, the
database on the volume included (its credentials are sealed). An update installs whatever npm
serves for that package and version. Update deliberately, and from a registry you trust.

## What llmr does not do

**It does not terminate TLS.** It serves plain HTTP. Anything crossing a network you do not
control needs a reverse proxy with TLS in front; [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) has
an example.

**It does not validate model output.** Anything a model returns is text somebody else
produced, tool call arguments included. Treat it as data, never as instruction.

**It does not rate limit or cap spending per caller yet.** Every caller that passes the token
check can use every enabled model.

**Retries are configured, not free.** A route retries a rate limit or a transient failure up
to its route set's `retry_attempts`. A timeout is not retried, because the provider may have
finished the work and billed for it.

## Supported versions

The latest release: the newest `v*` tag and its image. The project is pre 1.0, so fixes go
into a new release rather than being backported.
