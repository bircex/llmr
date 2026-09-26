# llmr

An LLM router that runs as a Docker container and is managed entirely over REST.

Your projects call one OpenAI-compatible endpoint. Behind it, llmr holds the providers
(Anthropic, OpenAI, Gemini, and anything OpenAI-compatible: Ollama, vLLM, Groq, OpenRouter),
which of their models are enabled, and which names route to which models, with fallbacks.
All of that is set through a management API, usually by a panel, and kept in an encrypted
database on a volume. There is no configuration file.

```
your apps ──OpenAI SDK──▶ llmr :8080 /v1 ──▶ anthropic/claude-sonnet-5
                                          └─▶ openai/gpt-5.1          (if the first is down)
your panel ──REST──────▶ llmr :8080 /manage   providers, models, routes, tests, status
```

## Run it

llmr runs only as its Docker image.

```sh
cp .env.example .env
docker run --rm ghcr.io/bircex/llmr keygen    # paste the output into .env as LLMR_MASTER_KEY
docker compose up -d
```

or without compose:

```sh
docker run -d --name llmr -p 127.0.0.1:8080:8080 \
  -v llmr-data:/var/lib/llmr \
  -e LLMR_MASTER_KEY="..." \
  -e LLMR_TOKEN="..." \
  ghcr.io/bircex/llmr:latest
```

| Environment | |
|---|---|
| `LLMR_MASTER_KEY` | **Required.** Seals every stored credential. `llmr keygen` makes one. Keep a copy: without it, stored credentials cannot be read, and llmr refuses to start with a different one |
| `LLMR_TOKEN` | Optional. Tokens callers must present as `Authorization: Bearer <token>`, comma separated. Unset, nothing is checked: keep the port on a network only your panel and apps can reach |
| `LLMR_LISTEN` | `0.0.0.0:8080` |
| `LLMR_MAX_BODY_MB` | `32`. Images arrive inline |
| `RUST_LOG`, `LLMR_LOG_FORMAT` | Log filter (`info`), and `json` for one object per line |

Everything llmr is told is kept in `/var/lib/llmr`. Mount a volume there, or it is lost with
the container.

## Set it up over REST

Four calls take an empty llmr to a served request:

```sh
# 1. A provider. The credential is encrypted at rest and never returned.
curl -X POST localhost:8080/manage/providers -d '{
  "id": "anthropic", "type": "anthropic", "credential": "sk-ant-..."
}'

# 2. Is the key good? Free: it asks for the model list, not for a completion.
curl -X POST localhost:8080/manage/providers/anthropic/test

# 3. What it serves, and enable one.
curl localhost:8080/manage/providers/anthropic/models
curl -X PUT localhost:8080/manage/providers/anthropic/models/claude-sonnet-5 -d '{"enabled": true}'

# 4. A name your apps call, with fallbacks.
curl -X PUT localhost:8080/manage/routes/default -d '{
  "routes": ["anthropic/claude-sonnet-5", "openai/gpt-5.1"]
}'
```

(With `LLMR_TOKEN` set, add `-H "Authorization: Bearer $LLMR_TOKEN"`.)

Changes take effect on the next request, without a restart. The full management API,
including provider types, capabilities for models llmr does not know, route policies and
status, is in [docs/MANAGEMENT.md](docs/MANAGEMENT.md).

## Connect a project

Anything that speaks OpenAI works:

```python
from openai import OpenAI

client = OpenAI(base_url="http://llmr:8080/v1", api_key="<LLMR_TOKEN, or anything if unset>")
reply = client.chat.completions.create(
    model="default",   # a route set, or an enabled provider/model such as "anthropic/claude-sonnet-5"
    messages=[{"role": "user", "content": "Hello"}],
)
```

A model that is not enabled is refused by name (`model_not_enabled`), not silently served.
Every reply says which provider answered (`x-llmr-route`) and whether anything failed first
(`x-llmr-fell-through`). [docs/API.md](docs/API.md) has the client API: fields, streaming,
errors.

## How it routes

A request is matched against what each route can actually do. A request with tools skips a
route that cannot take tools; one with an image skips a route that cannot see; one for a
route set marked `on_device` never leaves your hardware, even when every local route is
down. Among the routes that fit, the first that answers wins; a failing route is skipped for
a while rather than waited on in every request.

**A refusal stops.** When a model declines, the next one is not asked the same question.

**A field llmr cannot carry is refused, not dropped.** `n` above 1, `stop`, a forced
`tool_choice`: each is a `400` naming it, because a reply that ignored half the request is
still billed.

**Usage nobody reported is left out, never written as zero.**

## Documentation

| | |
|---|---|
| [docs/MANAGEMENT.md](docs/MANAGEMENT.md) | The management API: providers, models, route sets, tests, status |
| [docs/API.md](docs/API.md) | The client API: `/v1/chat/completions`, streaming, headers, errors |
| [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) | Compose, volumes, the master key, TLS in front, backups, logs |
| [SECURITY.md](SECURITY.md) | What llmr holds, where prompts go, how to report a problem |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How the code is laid out and what a pull request must pass |
| [docs/DESIGN.md](docs/DESIGN.md) | What was decided and why |
| [ROADMAP.md](ROADMAP.md) · [CHANGELOG.md](CHANGELOG.md) | What is next, and what changed |

## Not there yet

- **Usage reporting.** Every reply carries its token counts; per request, per model and
  overall totals with cost over the management API are next.
- **Command line providers.** Claude Code and Codex installed in the image, with their
  versions reported and updatable over the API.
- Bedrock (needs a SigV4 signing transport), an Anthropic Messages endpoint (`/v1/messages`),
  and embeddings.

## Releases

Every `vX.Y.Z` tag publishes `ghcr.io/bircex/llmr:X.Y.Z` and `:X.Y` for amd64 and arm64, and
a [GitHub release](https://github.com/bircex/llmr/releases) with the changelog.
`:latest` follows `main`. The image is the only thing llmr ships.

## License

MIT
