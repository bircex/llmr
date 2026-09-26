# Configuration

The gateway reads one TOML file: `--config PATH`, else `LLMR_CONFIG`, else `llmr.toml` in
the working directory. The image sets `LLMR_CONFIG=/etc/llmr/llmr.toml`.
[`llmr.example.toml`](../llmr.example.toml) is an annotated starting point.

Every table rejects keys it does not know, so a misspelt key is an error at startup rather
than a setting silently ignored. Check a file without serving anything:

```sh
llmr check --config llmr.toml
docker run --rm -v "$PWD/llmr.toml:/etc/llmr/llmr.toml:ro" --env-file .env ghcr.io/bircex/llmr:latest check
```

`check` parses the file, builds every provider (so a missing key is reported), lists routes
no request can ever select, asks every route whether it is reachable without a billable
request, and exits non zero if anything is wrong.

Keys are never written in the file. Each provider names the environment variable that holds
its key, and the gateway refuses to start when one is unset or empty. The file can be
committed and mounted read only.

## `[server]`

| Key | Default | Meaning |
|---|---|---|
| `listen` | `"0.0.0.0:8080"` | Address and port. `LLMR_LISTEN` overrides it |
| `auth` | `"keys"` | `"keys"`: callers must present a key from `api_keys_env`, and the gateway will not start with none. `"none"`: anybody who can reach the port can use it; only for a network nobody else is on |
| `api_keys_env` | `"LLMR_API_KEYS"` | The variable holding client keys, comma separated. Rotate by adding the new key, moving clients, then removing the old one |
| `max_body_mb` | `32` | Largest request body. Images arrive inline as base64 |
| `allow_direct` | `true` | Let a client ask for `provider/model` directly, as well as the names under `[[model]]` |
| `preflight` | `true` | At startup, ask every route whether it is reachable. Free: providers answer from their model list |

Clients present a key as `Authorization: Bearer <key>` or `x-api-key: <key>`.

## `[[provider]]`

Where a prompt can go, and whose key pays. Define as many as you like, including two of the
same kind with different keys.

| Key | Default | Meaning |
|---|---|---|
| `id` | required | The name routes use, as in `anthropic/claude-sonnet-5`. No `/` |
| `kind` | required | `anthropic`, `openai`, `gemini` or `openai-compatible` |
| `base_url` | the vendor's | Required for `openai-compatible`. For a vendor kind, set it to reach the same protocol somewhere else (a proxy, a regional endpoint) |
| `api_key_env` | see below | The variable holding the key |
| `reach` | `first-party-api` | Required for `openai-compatible`. Where the data goes: `first-party-api`, `cloud-partner`, `private-endpoint` or `self-hosted` |
| `timeout_secs` | `120` | How long one call may take |
| `shipped_models` | `true` | Start from the model table this release ships for the vendor kinds |

| `kind` | Protocol | Key variable by default |
|---|---|---|
| `anthropic` | Anthropic Messages | `ANTHROPIC_API_KEY` |
| `openai` | OpenAI chat completions, OpenAI's own endpoint | `OPENAI_API_KEY` |
| `gemini` | Gemini `generateContent` | `GEMINI_API_KEY` |
| `openai-compatible` | OpenAI chat completions anywhere else: Ollama, vLLM, LM Studio, Groq, Together, OpenRouter, LiteLLM | none; set `api_key_env` if the endpoint wants one |

**Why `reach` is required for `openai-compatible`.** A model on your own hardware and a hosted
API answer the same request at the same path, and nothing on the wire tells them apart. The
reach decides whether a name marked `on_device` may use the provider, so it is asked for
rather than guessed. Setting it wrong is silent.

**Prices.** For `anthropic`, `openai` and `gemini` at their own endpoint, the gateway knows the
vendor's published rates, which is what `order = "cheapest"` compares. A provider with a
`base_url` of its own, and every `openai-compatible` one, is unpriced: it sorts after every
priced route under `cheapest`, never before.

### `[[provider.model]]`

What a model can do, reached through this provider. For a vendor kind these rows add to or
replace rows of the shipped table. For `openai-compatible` they are the whole table.

A route to a model its provider does not list can never be chosen: nothing is known about
what it can do. The gateway says so at startup and `llmr check` fails on it.

| Key | Default | Meaning |
|---|---|---|
| `id` | required | The name the provider uses |
| `context_window` | `0` | Tokens that fit in one request, `0` when unknown |
| `max_output` | `0` | Most tokens in one reply, `0` when unknown |
| `tools` | `false` | Can be given tools |
| `structured_output` | `false` | Can be held to a JSON schema |
| `prompt_caching` | `false` | Repeated prefixes are cached |
| `thinking` | `false` | Can be asked to reason (`reasoning_effort`) |
| `images` | `false` | A request may carry an image |
| `streaming` | `false` | Replies arrive as they are written. Without it a streamed request still works, as one burst at the end |
| `source` | `"gateway configuration"` | Where these facts came from |
| `verified_at` | `""` | When somebody last checked them, `YYYY-MM-DD` |

Capabilities are what routing reads. A request with tools skips a route whose model row says
`tools = false`, so a row that under-claims makes a route unreachable for those requests, and
one that over-claims sends requests a model will ignore half of.

## `[[model]]`

The names clients ask for. A client sends `"model": "default"` and never names a vendor.

| Key | Default | Meaning |
|---|---|---|
| `name` | required | What a client puts in `model` |
| `routes` | required | `provider/model`, tried in order. Split at the first `/`, so `openrouter/meta-llama/llama-3.1-8b` is provider `openrouter`, model `meta-llama/llama-3.1-8b` |
| `order` | `"as-listed"` | `as-listed`, `cheapest` (lowest published rate first, unpriced last) or `healthiest` (fewest recent failures first) |
| `on_device` | `false` | Only `self-hosted` routes may serve it. A floor: when every local route is down the request fails rather than falling back |
| `retry_attempts` | `2` | Attempts per route, the first included. Only rate limits and transient failures are retried; a rate limit's own wait is honoured exactly. `1` means no retry |
| `breaker` | `true` | A route that keeps failing is skipped for a while (one second, doubling to a minute; five minutes for a rejected key) rather than tried first in every request |
| `deadline_secs` | none | Give up on the whole request after this long, however many routes are left |

Set `deadline_secs` for anything a person waits on. Without it, a provider that answers a
rate limit with a long `retry-after` makes the client wait that long too.

The requirement check always comes first: whatever `order` says, a route that cannot serve
the request is never chosen.

A client asking for `provider/model` directly (with `allow_direct`) gets a one route router
with two attempts and a breaker, for a model the provider lists.

## Environment

| Variable | Meaning |
|---|---|
| `LLMR_CONFIG` | Configuration path |
| `LLMR_LISTEN` | Overrides `server.listen` |
| `LLMR_API_KEYS` | Client keys, unless `api_keys_env` names another variable |
| `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY` | Provider keys, unless `api_key_env` names others |
| `RUST_LOG` | Log filter, `info` by default. `llmr=debug` for more |
| `LLMR_LOG_FORMAT` | `json` for one JSON object per line |

## A complete example

```toml
[server]
listen       = "0.0.0.0:8080"
api_keys_env = "LLMR_API_KEYS"

[[provider]]
id   = "anthropic"
kind = "anthropic"

[[provider]]
id   = "openai"
kind = "openai"

[[provider]]
id       = "local"
kind     = "openai-compatible"
base_url = "http://ollama:11434/v1"
reach    = "self-hosted"

[[provider.model]]
id             = "llama3.1:8b"
context_window = 128000
max_output     = 8192
tools          = true
streaming      = true

[[provider]]
id          = "openrouter"
kind        = "openai-compatible"
base_url    = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
reach       = "first-party-api"

[[provider.model]]
id        = "meta-llama/llama-3.1-70b-instruct"
tools     = true
streaming = true

[[model]]
name          = "default"
routes        = ["anthropic/claude-sonnet-5", "openai/gpt-5.1", "openrouter/meta-llama/llama-3.1-70b-instruct"]
deadline_secs = 60

[[model]]
name   = "cheap"
routes = ["anthropic/claude-haiku-4-5", "openai/gpt-5-mini"]
order  = "cheapest"

[[model]]
name      = "private"
routes    = ["local/llama3.1:8b"]
on_device = true
```
