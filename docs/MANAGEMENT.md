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
| `GET` `DELETE` | [`/manage/usage`](#usage) | Totals over a time range, overall or grouped; forget old rows |
| `GET` | `/manage/usage/requests` | Single requests, newest first |
| `GET` | [`/manage/clis`](#command-line-tools) | Claude Code, Codex, Gemini CLI: installed version, where it came from, the newest |
| `GET` | `/manage/clis/{name}` | One of them |
| `POST` | `/manage/clis/{name}/update` | Install a version inside the container |
| `POST` | `/manage/clis/{name}/reset` | Go back to the version in the image |

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
| `claude-code` | Claude Code, run inside the container ([below](#command-line-providers)) | an Anthropic API key |
| `codex` | OpenAI's Codex, run inside the container | an OpenAI API key |
| `gemini-cli` | Google's Gemini CLI, run inside the container | a Gemini API key |

`transport` is `api` for a provider llmr calls over HTTP, and `cli` for one it runs as a
program inside the container.

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

## Command line providers

`claude-code`, `codex` and `gemini-cli` run the vendor's own command line tool inside the
container, one process per request. The tools are installed in the image, ready; which
version each is at, and how to change it, is under [Command line tools](#command-line-tools).

```sh
curl -X POST localhost:8080/manage/providers -d '{
  "id": "claude", "type": "claude-code", "credential": "sk-ant-oat01-..."
}'
curl -X PUT localhost:8080/manage/providers/claude/models/claude-sonnet-5 -d '{"enabled": true}'
```

### Signing in with a subscription

Each tool takes the sign in of your own subscription (Claude Pro or Max, ChatGPT Plus or Pro,
a Google account with Gemini) as its `credential`, and its calls are then covered by the plan
rather than charged per token. An API key works too; llmr tells them apart by their shape.

| Type | Subscription `credential` | How to get it |
|---|---|---|
| `claude-code` | The token that starts `sk-ant-oat` | `claude setup-token` on any machine with Claude Code, then paste what it prints. Valid for a year |
| `codex` | The whole `auth.json`, as JSON text | `CODEX_HOME=$(mktemp -d) codex login` (add `--device-auth` on a machine with no browser), then paste `$CODEX_HOME/auth.json`. Sign in apart from your own Codex, as shown: the refresh token changes on every refresh, so two copies of one sign in log each other out |
| `gemini-cli` | The whole `oauth_creds.json`, as JSON text | Run `gemini`, choose "Login with Google", then paste `~/.gemini/oauth_creds.json` |

```sh
curl -X POST localhost:8080/manage/providers -d "$(jq -n --rawfile c "$CODEX_HOME/auth.json" \
  '{id: "codex", type: "codex", credential: $c}')"
```

A provider shows which it has: `"auth": "subscription"` (and `"credential": "subscription"`
in place of the last four characters) or `"auth": "api-key"`. A file that is not the one its
tool writes is refused with `param: "credential"` rather than stored as a key.

Codex and Gemini CLI refresh their sign in themselves. llmr gives each call the newest one,
reads it back when the call ends, and seals a refreshed one into the database in place of
the old. A Codex call that is about to refresh runs on its own, so two calls never spend the
same refresh token. Changing the credential through the API always wins over a refresh that
was in flight.

When a plan's limit is reached, the vendor answers with a rate limit, and the request falls
through to the next route in its set: put an API key provider after a subscription to keep
serving past the limit.

What else is different from an API provider:

| | |
|---|---|
| `credential` | A subscription's sign in, or the vendor's API key. Required. Subscription calls cost nothing per call (`subscription`); API key calls are priced at the vendor's API rates |
| `base_url` | Optional. Points the tool at a gateway or proxy that speaks the vendor's API; an API key provider is then unpriced |
| `reach` | Always `local-cli`: the tool runs here, and the prompt goes to the vendor |
| Models | Named by you, with the ids the tool accepts (`claude-sonnet-5`, `gpt-5.1`, `gemini-2.5-pro`). A tool cannot list them, so `listed` is `null` |
| Capabilities | None, and none may be set: text in, text out. A request with tools, an image or a schema skips these routes |
| Streaming | A streamed request works, and the reply arrives in one piece when the tool finishes |
| Speed | A process starts per request: a fraction of a second for Claude Code and Codex, about two for Gemini CLI |
| Concurrency | At most `LLMR_CLI_CONCURRENCY` calls (8 unless set) run at once, across every tool; the rest wait, and fall through to the next route if none frees in time |

**What a call can do.** Every tool is run with its own tools switched off: it cannot read
files, run commands, search the web or start sub agents, whatever the prompt asks. Each call
runs in an empty directory of its own that is removed afterwards, with only its own key in
its environment. The tool's own retries are off too, so a rate limit or an outage falls
through to the next route at once rather than being retried inside the tool.

**Testing one.** The free test can only say the tool is installed; no tool can check a key
without making a call. `{"model": "...", "live": true}` proves the key with one short request.

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

A command line provider's report also carries `cli`: the tool's version and where it came
from, as in [Command line tools](#command-line-tools).

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

## Usage

Every request the client API handles is recorded: answered, refused, failed or cut short.
A row holds what was asked for, the route that answered, the token counts, the cost, the
latency and the outcome. Never the prompt, never the reply. Rows are written in the
background, so they appear a moment after the reply.

### Totals

`GET /manage/usage?from=1790380800&to=1790467200&group_by=model`

| Parameter | |
|---|---|
| `from`, `to` | Unix seconds; `from` inclusive, `to` exclusive. Either may be left out |
| `group_by` | `none` (default), `model` (`provider/model`), `provider`, `asked` (the name the client used), `day` (UTC) |
| `provider`, `model`, `asked`, `outcome` | Narrow to one. `outcome` is `ok`, `refused`, `error` or `interrupted` |

```json
{
  "from": 1790380800, "to": 1790467200, "group_by": "model",
  "total": {
    "requests": 1240,
    "outcomes": { "ok": 1198, "refused": 2, "error": 37, "interrupted": 3 },
    "tokens": { "input": 812004, "cache_read": 2210400, "cache_write": 48000, "output": 190233, "total": 3260637 },
    "usage_missing": 0,
    "latency_ms_avg": 1840,
    "cost": [{ "currency": "USD", "amount": "14.281950" }],
    "cost_complete": false,
    "priced": 1150, "partial": 0, "unpriced": 12, "free": 39, "subscription": 0
  },
  "data": [
    { "model": "anthropic/claude-sonnet-5", "requests": 1150, "...": "..." },
    { "model": "ollama/llama3.1:8b", "requests": 39, "free": 39, "cost": [], "...": "..." },
    { "model": null, "requests": 37, "outcomes": { "error": 37, "...": 0 }, "...": "..." }
  ]
}
```

How to read the cost:

| | |
|---|---|
| `cost` | One amount per currency, never added across currencies |
| `priced` | Requests priced in full: the provider's published rate times the usage it reported |
| `partial` | Priced, but the provider left some usage fields out, so the amount is a floor |
| `unpriced` | Answered by a paid provider llmr has no rate for (a custom `base_url`, an `openai-compatible` host, a model newer than the price table), or whose provider reported no usage |
| `free` | Answered by a `self-hosted` provider: tokens are counted, nothing is charged |
| `subscription` | Answered by a command line tool signed in with a subscription: tokens are counted, the plan covers them |
| `cost_complete` | `true` only when no request was `partial` or `unpriced`. Otherwise the amounts are what is known, and the real bill is higher |
| `usage_missing` | Answered requests whose provider reported no token counts at all |

Requests that failed before any provider answered (a `model_not_found`, every route down)
have no cost and no tokens; they are counted in `outcomes.error` and grouped under a `null`
model.

### Single requests

`GET /manage/usage/requests?limit=100&before=48213`, newest first, with the same filters.

```json
{
  "data": [{
    "id": 48212, "at": 1790466950, "request_id": "chatcmpl-18d9...", "asked": "default",
    "route": "anthropic/claude-sonnet-5", "provider": "anthropic", "model": "claude-sonnet-5",
    "served_model": "claude-sonnet-5", "stream": true, "outcome": "ok", "error_code": null,
    "stop_reason": "end_turn", "attempts": 1, "fell_through": 0, "latency_ms": 2210,
    "tokens": { "input": 812, "cache_read": 0, "cache_write": 0, "output": 64, "total": 876 },
    "cost": { "status": "priced", "amount": "0.003396", "currency": "USD" }
  }],
  "next_before": 48212
}
```

`limit` is 1 to 1000 (default 100). Pass `next_before` as `before` for the next, older page;
it is `null` on the last one. `request_id` is the `id` the client received, so a client's log
and this one can be joined.

### Forgetting old rows

`DELETE /manage/usage?before=1782604800` removes rows recorded before that time and answers
`{"deleted": 18231}`. `before` is required.

## Command line tools

The image carries Claude Code, Codex and Gemini CLI at the versions the release was tested
with. Any of them can be moved to another version from npm, inside the running container:
the new copy is installed onto the data volume, checked, then swapped in, and it survives
restarts and upgrades of the image until it is reset.

`GET /manage/clis`

```json
{ "data": [
  { "name": "claude-code", "title": "Claude Code", "program": "claude",
    "package": "@anthropic-ai/claude-code", "provider_type": "claude-code",
    "installed": true, "version": "2.1.283", "source": "image", "image_version": "2.1.283" },
  { "name": "codex", "...": "...", "version": "0.158.0", "source": "updated", "image_version": "0.157.1" }
] }
```

`source` is `image` for the tested version the image carries, `updated` for one installed
through this API. Add `?check_latest=true` to ask npm for the newest version as well; each
tool then carries `latest` and `update_available`, or `latest_error` when the registry could
not be reached. `GET /manage/clis/{name}` is one tool, with the same parameter.

### Updating

`POST /manage/clis/{name}/update`

```json
{ "version": "latest" }
```

`version` is `latest` (the default, when there is no body) or a version number such as
`2.1.290`; anything else is refused. The call returns when the install is done, usually within
a minute, with the tool as above plus `previous_version` and `took_ms`. The next request runs
the new copy; requests already running finish on the old one.

A version other than the image's has not been tested with this release. The tools change the
shape of what they print from time to time, and a version llmr cannot read shows up as failed
requests on its routes, not as a failed update. Update one provider's tool, send a live test,
and reset if it fails.

One update runs at a time; a second while one runs is a `409`. The container needs to reach
the npm registry, or the one in `LLMR_NPM_REGISTRY`.

### Going back

`POST /manage/clis/{name}/reset` removes the copy on the volume, so the image's version is
used again. `removed_update` says whether there was one.

## Errors

| Status | `code` | |
|---|---|---|
| 400 | `invalid_request` | A field is missing or wrong; `param` names it |
| 401 | `invalid_api_key` | `LLMR_TOKEN` is set and the call did not present it |
| 404 | `not_found` | No such provider, model setting or route set |
| 409 | `conflict` | A provider with that id exists already, or a tool update is already running |
| 502 | `update_failed` | npm could not install the version, or the installed tool did not start. The copy in use is unchanged |
| 500 | `internal_error` | The database could not be read or written |
