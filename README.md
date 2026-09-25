# llmr

An LLM router you run as a container. Every project talks to one URL in the OpenAI chat
completions shape, and the gateway decides which provider answers: Anthropic, OpenAI,
Gemini, or anything OpenAI-compatible such as Ollama, vLLM, Groq or OpenRouter.

```
your app ──OpenAI SDK──▶ llmr :8080 ──▶ anthropic/claude-sonnet-5
                                    └─▶ openai/gpt-5.1           (if the first is down)
                                    └─▶ local/llama3.1:8b         (the only choice for "private")
```

Changing a vendor, adding a fallback or moving a workload onto your own hardware is an edit
to one file on the gateway. No project changes a line.

## Run it

```sh
cp llmr.example.toml llmr.toml     # the routes; keys are never written here
cp .env.example .env               # LLMR_API_KEYS and the provider keys
docker compose up -d
```

or without compose:

```sh
docker run -d --name llmr -p 8080:8080 \
  -v "$PWD/llmr.toml:/etc/llmr/llmr.toml:ro" \
  --env-file .env \
  ghcr.io/bircex/llmr:latest
```

Check a configuration before deploying it. This builds every provider, reports any route
that can never be chosen, and asks each one whether it is reachable, without sending a
billable request:

```sh
docker run --rm -v "$PWD/llmr.toml:/etc/llmr/llmr.toml:ro" --env-file .env \
  ghcr.io/bircex/llmr:latest check
```

The image is distroless, runs as a non root user, has a built in healthcheck, and stops
cleanly on `docker stop`, finishing requests already in flight.

## Connect a project

Anything that speaks OpenAI works. Point it at the gateway and give it one of the keys in
`LLMR_API_KEYS`:

```sh
OPENAI_BASE_URL=http://localhost:8080/v1
OPENAI_API_KEY=<one of LLMR_API_KEYS>
```

```python
from openai import OpenAI

client = OpenAI(base_url="http://localhost:8080/v1", api_key="...")
reply = client.chat.completions.create(
    model="default",   # a name from llmr.toml, not a vendor model
    messages=[{"role": "user", "content": "Hello"}],
)
```

```ts
import OpenAI from "openai";

const client = new OpenAI({ baseURL: "http://localhost:8080/v1", apiKey: "..." });
const stream = await client.chat.completions.create({
  model: "default",
  messages: [{ role: "user", content: "Hello" }],
  stream: true,
});
```

From another compose project, join the `llmr` network and use `http://llmr:8080/v1`.

## Configure it

One TOML file, [`llmr.example.toml`](llmr.example.toml) is the annotated version.

```toml
[server]
api_keys_env = "LLMR_API_KEYS"

[[provider]]
id   = "anthropic"
kind = "anthropic"                 # reads ANTHROPIC_API_KEY

[[provider]]
id   = "openai"
kind = "openai"                    # reads OPENAI_API_KEY

[[provider]]
id       = "local"
kind     = "openai-compatible"
base_url = "http://ollama:11434/v1"
reach    = "self-hosted"           # required: nothing on the wire says where it runs

[[provider.model]]
id        = "llama3.1:8b"
tools     = true
streaming = true

[[model]]
name   = "default"
routes = ["anthropic/claude-sonnet-5", "openai/gpt-5.1"]

[[model]]
name      = "private"
routes    = ["local/llama3.1:8b"]
on_device = true
```

**Providers** are where a prompt can go and whose key pays. Keys come from environment
variables, never from the file, and a missing one stops the gateway at startup rather than
on the first request. The vendor kinds ship a dated table of the models this release knows
and what each can do; an `openai-compatible` endpoint lists its own.

**Models** are the names clients ask for. Each is an ordered list of `provider/model`
routes and a policy:

| Key | Default | Meaning |
|---|---|---|
| `routes` | | Tried in order until one answers |
| `order` | `as-listed` | Or `cheapest` (published rate, unpriced last) or `healthiest` (fewest recent failures) |
| `on_device` | `false` | Only `self-hosted` routes may serve it. A floor: no fallback relaxes it |
| `retry_attempts` | `2` | Attempts per route for a failure worth repeating; a rate limit's own wait is honoured |
| `breaker` | `true` | A route that keeps failing is skipped for a while instead of waited on in every request |
| `deadline_secs` | none | Give up on the whole request after this long. Set it for anything a person waits on: a rate limit's `retry-after` is honoured in full, and without a deadline that wait is the client's too |

With `allow_direct = true` (the default) a client may also ask for `provider/model`, such
as `anthropic/claude-haiku-4-5`, for a model the provider knows.

| Environment | Meaning |
|---|---|
| `LLMR_CONFIG` | Configuration path. `/etc/llmr/llmr.toml` in the image |
| `LLMR_LISTEN` | Overrides `server.listen` |
| `LLMR_API_KEYS` | Keys clients may present, comma separated. Add a new key, move clients, remove the old one |
| `RUST_LOG` | Log filter, `info` by default |
| `LLMR_LOG_FORMAT` | `json` for one object per line |

## How it routes

A request is matched against what each route can actually do, reached that way. A request
with tools skips a route that cannot take tools; one with an image skips a route that cannot
see; one for a name marked `on_device` never leaves your hardware, even when every local
route is down. Among the routes that fit, the first that answers wins.

**A refusal stops.** When a model declines, the next one is not asked the same question.
The client gets a normal reply with `finish_reason: "content_filter"`.

**A stream falls through only before its first byte.** After that, a failure arrives inside
the stream and no `[DONE]` follows, rather than a second model continuing half a sentence it
did not write.

Every reply says what happened on the way:

| Header | |
|---|---|
| `x-llmr-route` | The `provider/model` that answered |
| `x-llmr-attempts` | Calls made, retries included |
| `x-llmr-fell-through` | Routes skipped or failed before it. Non zero on a successful call is a provider degrading while nothing is failing |

A client can tighten the privacy floor for one request with `x-llmr-on-device: true`. It
cannot loosen one.

## Endpoints

| | | |
|---|---|---|
| `POST` | `/v1/chat/completions` | Whole or streamed (`stream: true`, `stream_options.include_usage`) |
| `GET` | `/v1/models` | The names from `llmr.toml` |
| `GET` | `/llmr/routes` | Every name, its routes, what each can do, and which are resting |
| `GET` | `/healthz` | Liveness, no key needed |

## What is refused rather than dropped

A field the gateway cannot carry to every provider is a `400` naming it, never silently
ignored, because a reply that ignored half the request is still billed: `n` above 1, `stop`,
non zero penalties, `logprobs`, `logit_bias`, `tool_choice` other than `auto`,
`response_format: json_object` (send `json_schema`), audio, and an image link whose type
cannot be read from its extension (send a data URL).

Usage a provider did not report is left out of the reply rather than written as zero.

## Known gaps

- Bedrock and the command line providers (Claude Code, Codex) exist in the engine and are
  not yet configurable here: Bedrock needs a SigV4 signing transport, and a vendor CLI inside
  a container has no login to use.
- Only the OpenAI shape is served. An Anthropic Messages endpoint (`/v1/messages`) would
  carry prompt caching and thinking signatures that this shape has no place for.
- The engine's spending cap (`Budget`) is per process lifetime, which fits a batch run and
  not a long running server, so it is not exposed yet. Per key budgets and rate limits are
  the natural next step.
- No embeddings endpoint yet, though the engine has two embedders.

## Build from source

```sh
cargo run --features server -- --config llmr.toml
docker build -t llmr .
```

The routing engine underneath is also a Rust crate; [LIBRARY.md](LIBRARY.md) describes it,
and [docs/DESIGN.md](docs/DESIGN.md) records why it is shaped the way it is.

## License

MIT
