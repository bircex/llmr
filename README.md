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
cleanly on `docker stop`, finishing requests already in flight. It serves plain HTTP: before
exposing it beyond the host, put TLS in front. [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) covers
that, binaries without Docker, key rotation and logs.

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

**Models** are the names clients ask for: an ordered list of `provider/model` routes, and a
policy for them (`order`, `on_device`, `retry_attempts`, `breaker`, `deadline_secs`).

Every key, default and environment variable is in
[docs/CONFIGURATION.md](docs/CONFIGURATION.md).

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

Every reply says what happened on the way: `x-llmr-route` is the `provider/model` that
answered, and a non zero `x-llmr-fell-through` on a successful call is a provider degrading
while nothing is failing.

A field the gateway cannot carry to every provider (`n` above 1, `stop`, a forced
`tool_choice`, `json_object`, ...) is a `400` naming it, never silently ignored, because a
reply that ignored half the request is still billed. Usage a provider did not report is left
out rather than written as zero.

[docs/API.md](docs/API.md) has the endpoints, every accepted and refused field, streaming,
and the error codes.

## Documentation

| | |
|---|---|
| [docs/CONFIGURATION.md](docs/CONFIGURATION.md) | Every configuration key, with defaults and a complete example |
| [docs/API.md](docs/API.md) | The HTTP API: endpoints, fields, streaming, headers, errors |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Compose, plain Docker, binaries and systemd, TLS in front, keys, logs |
| [SECURITY.md](SECURITY.md) | What the gateway holds, where prompts go, and how to report a problem |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How the code is laid out and the checks a pull request passes |
| [docs/ENGINE.md](docs/ENGINE.md) | The routing engine inside the gateway |
| [docs/DESIGN.md](docs/DESIGN.md) | What was decided and why |
| [ROADMAP.md](ROADMAP.md) · [CHANGELOG.md](CHANGELOG.md) | What is next, and what changed |

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

## Releases

Every `vX.Y.Z` tag publishes:

- the image, `ghcr.io/bircex/llmr:X.Y.Z` and `:X.Y`, for amd64 and arm64 (`:latest` follows
  `main`);
- a [GitHub release](https://github.com/bircex/llmr/releases) with binaries for Linux
  (x86_64, arm64) and macOS (arm64), each with a checksum, and the changelog section as
  notes.

llmr is not published to crates.io. It is a service, not a library; 0.1.0 on crates.io
predates the gateway and nothing newer goes there. What is versioned is what you depend on:
the HTTP API, the response headers, the configuration file and the command line.

## Build from source

```sh
cargo run --release --features server -- --config llmr.toml
docker build -t llmr .
```

Contributions are welcome; [CONTRIBUTING.md](CONTRIBUTING.md) is the place to start.

## License

MIT
