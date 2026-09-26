# Management API

Everything llmr serves is set here: providers, the models each serves, and the route sets
clients ask for. A panel drives it; `curl` works too. Changes are written to the database
and take effect on the next request, with no restart.

All paths are under `/manage`, take and return JSON, and use the same error envelope as the
client API (`{"error": {"message", "type", "param", "code"}}`). With `LLMR_TOKEN` set, every
call needs `Authorization: Bearer <token>` (or `x-api-key`); without it, nothing is checked.

| Method | Path | |
|---|---|---|
| `GET` | [`/manage/status`](#status) | Version, uptime, counts, providers that cannot be built |
| `GET` | [`/manage/provider-types`](#provider-types) | What kinds of provider exist and what each needs |
| `GET` `POST` | [`/manage/providers`](#providers) | List, add |
| `GET` `PATCH` `DELETE` | `/manage/providers/{id}` | Read, change, remove |
| `POST` | [`/manage/providers/{id}/test`](#testing-a-provider) | Is it reachable, is the key good; optionally one real call |
| `GET` | [`/manage/providers/{id}/models`](#models) | What it serves, what each can do, what is enabled |
| `PUT` `DELETE` | `/manage/providers/{id}/models/{model}` | Enable, disable, set capabilities; forget |
| `GET` | `/manage/models` | Every enabled model, across providers |
| `GET` | [`/manage/routes`](#route-sets) | Every route set, with what is usable and what is resting |
| `GET` `PUT` `DELETE` | `/manage/routes/{name}` | One route set |

## Status

`GET /manage/status`

```json
{
  "version": "0.2.0",
  "uptime_secs": 3600,
  "auth": true,
  "providers": { "total": 3, "enabled": 2, "problems": [{ "provider": "groq", "problem": "no credential is set, and this provider type needs one" }] },
  "models_enabled": 5,
  "route_sets": 2
}
```

`problems` lists enabled providers that could not be built. Their routes are out of service
until the problem is fixed; everything else keeps serving.

## Provider types

`GET /manage/provider-types`: what a panel needs to render an "add provider" form.

```json
{ "data": [
  { "id": "anthropic", "name": "Anthropic", "transport": "api",
    "default_base_url": "https://api.anthropic.com", "reach_required": false,
    "default_reach": "first-party-api", "credential": "required", "lists_models": true, "priced": true },
  { "id": "openai-compatible", "name": "OpenAI-compatible endpoint", "transport": "api",
    "default_base_url": null, "reach_required": true,
    "default_reach": null, "credential": "optional", "lists_models": true, "priced": false }
] }
```

| Type | For | Credential |
|---|---|---|
| `anthropic` | Anthropic's API | required |
| `openai` | OpenAI's API | required |
| `gemini` | Google's Gemini API | required |
| `openai-compatible` | Ollama, vLLM, LM Studio, Groq, Together, OpenRouter, LiteLLM, anything on `/v1/chat/completions` | optional |

`priced` says whether llmr knows the provider's published prices, which is what a route
set's `order: "cheapest"` compares. A provider with a `base_url` of its own is unpriced.

## Providers

### Add one

`POST /manage/providers` → `201`

```json
{
  "id": "ollama",
  "type": "openai-compatible",
  "base_url": "http://ollama:11434/v1",
  "reach": "self-hosted",
  "credential": null,
  "timeout_secs": 120,
  "enabled": true
}
```

| Field | | |
|---|---|---|
| `id` | required | 1 to 64 letters, digits, `-`, `_`, `.`. Routes name it: `ollama/llama3.1:8b` |
| `type` | required | One of the provider types |
| `base_url` | when the type has no default | `http://` or `https://`. For a vendor type, set it to reach the same protocol elsewhere (a proxy, a region) |
| `reach` | when the type requires it | Where the data goes: `first-party-api`, `cloud-partner`, `private-endpoint`, `self-hosted` |
| `credential` | when the type requires it and the provider is enabled | The API key. Encrypted before it is written, never returned |
| `timeout_secs` | `120` | 1 to 3600. How long one call may take |
| `enabled` | `true` | A disabled provider serves nothing and can still be tested |

**Why `reach` is required for `openai-compatible`.** A model on your own hardware and a hosted
API answer the same request at the same path, and nothing on the wire tells them apart. The
reach decides whether a route set marked `on_device` may use the provider, so it is asked for
rather than guessed. Setting it wrong is silent.

A provider as returned:

```json
{
  "id": "anthropic", "type": "anthropic", "transport": "api",
  "base_url": "https://api.anthropic.com", "reach": "first-party-api",
  "timeout_secs": 120, "enabled": true,
  "credential": "…a1b2",
  "created_at": 1790460000, "updated_at": 1790460000
}
```

`credential` is the last four characters, or `null` when there is none. A `problem` field
appears when the provider is enabled and cannot be built.

### Change one

`PATCH /manage/providers/{id}` with any of `base_url`, `reach`, `timeout_secs`, `enabled`,
`credential`. `null` resets `base_url` and `reach` to the type's default and removes the
credential; a field left out is left alone. The type and id cannot change: delete and add.

Rotating a key is `PATCH {"credential": "sk-new..."}`. The next request uses it.

### Remove one

`DELETE /manage/providers/{id}` → `204`. Its model settings go with it. Route sets that name
it stay, with those routes reported as unavailable.

## Testing a provider

`POST /manage/providers/{id}/test`, with an optional body. Works on a disabled provider, so a
key can be checked before anything is switched on.

| Body | What happens | Cost |
|---|---|---|
| none | Fetches the model list | free |
| `{"model": "claude-sonnet-5"}` | Checks that this model is in the list the key can reach | free |
| `{"model": "claude-sonnet-5", "live": true}` | The above, then one real request asking for one word | a few tokens |

```json
{
  "provider": "anthropic",
  "access": "ready",
  "models_listed": 9,
  "live": { "ok": true, "latency_ms": 812, "model": "claude-sonnet-5", "usage": { "input_tokens": 14, "output_tokens": 2 } }
}
```

`access` has three answers, and the difference matters:

| | |
|---|---|
| `ready` | Nothing found that would stop a call |
| `denied` | The provider said no: a rejected key, a model this account cannot reach. It stays no until somebody fixes it. `detail` says what was said |
| `unknown` | Nothing could be established: a timeout, a 503, a provider that cannot list its models. Not a refusal |

## Models

### What a provider serves

`GET /manage/providers/{id}/models`

```json
{
  "provider": "anthropic",
  "listing_error": null,
  "data": [
    { "id": "claude-sonnet-5", "route": "anthropic/claude-sonnet-5", "enabled": true, "listed": true,
      "capabilities": { "context_window": 1000000, "max_output": 128000, "tools": true, "structured_output": true,
                        "prompt_caching": true, "thinking": true, "images": true, "streaming": true },
      "capabilities_source": "shipped", "updated_at": 1790460000 }
  ]
}
```

The list merges three sources: what the provider reports right now (`listed`), the table of
models this release knows (`capabilities_source: "shipped"`), and models you set yourself
(`"custom"`). `listed` is `null` when the provider could not be asked, and `listing_error`
says why.

### Enable, disable, describe

`PUT /manage/providers/{id}/models/{model}`

```json
{ "enabled": true }
```

A model is only served when it is enabled. For a model llmr knows, that is all it takes. For
one it does not (anything behind `openai-compatible`, or a vendor model newer than this
release), say what it can do, or it is refused with `param: "capabilities"`:

```json
{
  "enabled": true,
  "capabilities": { "context_window": 128000, "max_output": 8192, "tools": true, "streaming": true }
}
```

| Capability | Default | Meaning |
|---|---|---|
| `context_window`, `max_output` | `0` | Tokens in one request and one reply, `0` when unknown |
| `tools` | `false` | Can be given tools |
| `structured_output` | `false` | Can be held to a JSON schema |
| `prompt_caching` | `false` | Repeated prefixes are cached |
| `thinking` | `false` | Can be asked to reason (`reasoning_effort`) |
| `images` | `false` | A request may carry an image |
| `streaming` | `false` | Replies arrive as written. Without it a streamed request still works, as one burst at the end |

Capabilities are what routing reads: a request with tools skips a model with `tools: false`.
Under-claiming makes a model unreachable for those requests; over-claiming sends it requests
it will half ignore. `"capabilities": null` removes a custom set and goes back to the shipped
one.

Model ids may contain slashes: `PUT /manage/providers/openrouter/models/meta-llama/llama-3.1-70b`.

`DELETE /manage/providers/{id}/models/{model}` forgets the model's settings.

### Every enabled model

`GET /manage/models`: every model a client can call directly as `provider/model`.

```json
{ "data": [
  { "id": "anthropic/claude-sonnet-5", "provider": "anthropic", "model": "claude-sonnet-5",
    "type": "anthropic", "reach": "first-party-api", "priced": true, "capabilities": { "...": "..." } }
] }
```

## Route sets

A route set is a name clients ask for, such as `default`, and the models behind it in order.

`PUT /manage/routes/{name}`

```json
{
  "routes": ["anthropic/claude-sonnet-5", "openai/gpt-5.1", "ollama/llama3.1:8b"],
  "order": "as-listed",
  "on_device": false,
  "retry_attempts": 2,
  "breaker": true,
  "deadline_secs": 60
}
```

| Field | Default | |
|---|---|---|
| `routes` | required | `provider/model`, tried in order. Split at the first `/` |
| `order` | `as-listed` | `as-listed`, `cheapest` (lowest published rate first, unpriced last) or `healthiest` (fewest recent failures first) |
| `on_device` | `false` | Only `self-hosted` routes may serve it. A floor: when every local route is down the request fails rather than falling back |
| `retry_attempts` | `2` | Attempts per route, the first included. Only rate limits and transient failures are retried, and a rate limit's own wait is honoured exactly |
| `breaker` | `true` | A route that keeps failing is skipped for a while (a second, doubling to a minute; five minutes for a rejected key) |
| `deadline_secs` | none | Give up on the whole request after this long. Set it for anything a person waits on |

The name is 1 to 64 letters, digits, `-`, `_`, `.`; a `/` would read as `provider/model`.

A route set as returned adds what llmr made of it:

```json
{
  "name": "default",
  "routes": ["anthropic/claude-sonnet-5", "openai/gpt-5.1", "ollama/llama3.1:8b"],
  "...": "...",
  "usable": ["anthropic/claude-sonnet-5", "ollama/llama3.1:8b"],
  "unavailable": [{ "route": "openai/gpt-5.1", "why": "the model is not enabled" }],
  "resting": [{ "route": "anthropic/claude-sonnet-5", "seconds_left": 12 }]
}
```

A route is `unavailable` when its provider does not exist, is disabled or cannot be built,
when its model is not enabled, or when nothing is known about what the model can do. It is
`resting` when a breaker is skipping it after failures. A route set with nothing usable
answers `400 no_route`.

`GET /manage/routes` lists them all; `DELETE /manage/routes/{name}` removes one.

## Errors

| Status | `code` | |
|---|---|---|
| 400 | `invalid_request` | A field is missing or wrong; `param` names it |
| 401 | `invalid_api_key` | `LLMR_TOKEN` is set and the call did not present it |
| 404 | `not_found` | No such provider, model setting or route set |
| 409 | `conflict` | A provider with that id exists already |
| 500 | `internal_error` | The database could not be read or written |
