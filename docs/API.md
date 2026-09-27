# Client API

llmr speaks the OpenAI chat completions shape, so any OpenAI SDK or framework works by
changing its base URL to `http://<host>:8080/v1`. This page is what it accepts, what it
refuses, and what it sends back. Setting up what it serves is the
[management API](MANAGEMENT.md).

| Method | Path | Token | |
|---|---|---|---|
| `POST` | `/v1/chat/completions` | when set | A chat call, whole or streamed |
| `GET` | `/v1/models` | when set | The names a client can use |
| `GET` | `/healthz` | never | Liveness: `200 ok` while the process serves |

With `LLMR_TOKEN` set, a call presents one of its tokens as `Authorization: Bearer <token>`
or `x-api-key: <token>`. OpenAI SDKs send their `api_key` as the first, so setting the SDK's
key to the token is all it takes. Without `LLMR_TOKEN`, nothing is checked.

## `POST /v1/chat/completions`

### `model`

A route set, such as `default`, or an enabled model addressed directly as `provider/model`,
such as `anthropic/claude-haiku-4-5`. A model that exists and is not enabled is a
`404 model_not_enabled` saying so; any other name is a `404 model_not_found`.

The reply's `model` is the model that actually answered, not the name asked for.

### What is carried

| Field | Notes |
|---|---|
| `messages` | `system`, `developer`, `user`, `assistant` and `tool` roles. `system` and `developer` text becomes the system prompt. Content as a string or as `text` parts |
| images | `image_url` parts in a user message, as a `data:` URL or an `http(s)` link ending in `.png`, `.jpg`, `.jpeg`, `.gif` or `.webp` |
| `tools` | Function tools. `tool_calls` in assistant messages and `tool` results are carried both ways |
| `tool_choice` | `"auto"` only |
| `response_format` | `text`, or `json_schema` with a schema |
| `reasoning_effort` | `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max` |
| `max_completion_tokens`, `max_tokens` | The newer one wins when both are set |
| `temperature`, `top_p` | |
| `stream`, `stream_options.include_usage` | See streaming below |
| `n` | `1` only |
| `user`, `seed`, `metadata`, `store`, `parallel_tool_calls` | Accepted and not sent: they cannot change what the reply owes you |

Consecutive `tool` messages, and a `user` message straight after them, become one user turn,
which is the only arrangement every provider accepts.

### What is refused

A field llmr cannot carry to every provider is a `400` naming it in `error.param`,
never silently dropped, because a reply that ignored half the request is still billed:

- `n` above 1
- `stop`
- `presence_penalty` or `frequency_penalty` other than 0
- `logprobs: true`, `logit_bias`
- `tool_choice` other than `"auto"`
- `response_format: {"type": "json_object"}`; send `json_schema` with the shape you want
- `audio`, `modalities`, `prediction`, `web_search_options`, the legacy `functions`
- an image link whose type cannot be read from its extension; send a data URL
- a content part other than text or an image

### Choosing a route

The request is matched against what each route can do. A request with tools skips routes
that cannot take tools; one with an image skips routes that cannot see; one with a schema or a
`reasoning_effort` other than `none` skips routes without structured output or reasoning. Among the routes
that fit, they are tried in the route set's order until one answers.

A route set marked `on_device` is only served by `self-hosted` routes. A client can ask for the
same floor on one request with the header `x-llmr-on-device: true`; a header can tighten the
floor and never loosen it.

When no route can serve the request, the answer is `400 no_route` and the message says what
each route was missing.

### Reply

The standard `chat.completion` object, with two additions:

- `choices[0].llmr_stop_reason`: the engine's own stop reason (`end_turn`, `tool_use`,
  `stop_sequence`, `max_tokens`, `refusal`, `pause_turn`, `context_window_exceeded`,
  `interrupted`, `other`). `finish_reason` has five words and some of these would read as
  finished when they are not.
- `choices[0].message.reasoning_content`: reasoning text, when the model showed any.
- `llmr_cost`: what this request cost. `{"status": "priced", "amount": "0.003396", "currency":
  "USD"}`; `"partial"` when the provider left some usage out and the amount is a floor;
  `{"status": "unpriced"}` for a paid provider llmr has no rate for; `{"status": "free"}` for a
  self hosted model. Never a zero standing in for "unknown".

`usage` is present only when the provider reported both prompt and output counts. It is
never filled with zeros, because a zero turns an unknown cost into a free one.
`prompt_tokens` includes cached tokens, and `prompt_tokens_details.cached_tokens` says how many.

**A refusal is a successful reply.** When a model declines, the reply is a `200` with
`content: null`, `refusal` set and `finish_reason: "content_filter"`, which is how this shape
writes one. The next route is not asked the same question.

### Headers on every reply

| Header | |
|---|---|
| `x-llmr-route` | The `provider/model` that answered |
| `x-llmr-attempts` | Calls made, retries included |
| `x-llmr-cost` | The amount and currency, `0.003396 USD`, when the request was priced. Absent otherwise |
| `x-llmr-fell-through` | Entries for routes skipped or failed before the one that answered, one per failed attempt. Non zero on a successful call is a provider degrading while nothing is failing |

### Streaming

`stream: true` answers with server sent events: `chat.completion.chunk` objects, then
`data: [DONE]`. With `stream_options.include_usage`, a last chunk before `[DONE]` carries
`usage` and `llmr_cost` with empty `choices`, when the provider reported usage.

- **A route is replaced only before the first byte.** A provider that fails while the stream
  opens falls through to the next route, and the failure is an ordinary HTTP error status.
  After the first event nothing is replaced: a second model continuing half a sentence it
  did not write would be text nobody wrote.
- **A stream that breaks says so.** An error after the first event arrives as a final
  `data: {"error": {...}}` frame, and no `[DONE]` follows. The same is true when the provider
  ends its stream without saying why the model stopped. A client that waits for `[DONE]`
  never treats a cut-off reply as finished.
- **Quiet streams are kept open.** A `: keep-alive` comment is sent after 15 seconds without
  an event, so a proxy does not close a stream while a model thinks.
- **A client that leaves stops the call.** The upstream connection is closed at once rather
  than paid for to the end.
- A route that cannot really stream still answers a streamed request, with the whole reply
  as one burst at the end.

## Errors

Every error is the OpenAI envelope:

```json
{ "error": { "message": "...", "type": "...", "param": null, "code": "..." } }
```

| Status | `code` | Meaning |
|---|---|---|
| 400 | `invalid_request` | The request is malformed or asks for something refused above; `param` names the field |
| 400 | `no_route` | No usable route in the set can serve this request |
| 400 | `upstream_rejected_request` | The provider refused the request's shape |
| 401 | `invalid_api_key` | `LLMR_TOKEN` is set and the call did not present one of its tokens |
| 404 | `model_not_found` | Not a route set or a known model |
| 404 | `model_not_enabled` | A model that exists and is switched off |
| 404 | `not_found` | No such endpoint |
| 429 | `rate_limited` | Every route that was tried is rate limited. `Retry-After` carries the provider's wait, rounded up |
| 502 | `upstream_credential_rejected` | A provider rejected **llmr's** key for it. Your token was fine; the provider's credential needs fixing through the management API |
| 502 | `upstream_not_found` | A provider does not have the model it was asked for |
| 502 | `upstream_unreadable` | A provider answered and the answer could not be read |
| 503 | `upstream_unavailable` | Every route that could serve this failed or is resting |
| 504 | `timeout` | The configured deadline passed |

The split is by whose problem it is: a 4xx is something the client can change, a 5xx is
behind llmr.

## `GET /v1/models`

```json
{ "object": "list", "data": [
  { "id": "default", "object": "model", "created": 0, "owned_by": "llmr" },
  { "id": "anthropic/claude-sonnet-5", "object": "model", "created": 0, "owned_by": "llmr" }
] }
```

Route sets first, because those are the names a client is meant to use, then every enabled
model as `provider/model`. What each route set is made of, and which of its routes are
usable or resting, is `GET /manage/routes`.

## `GET /healthz`

`200 ok`, no key. The image's `HEALTHCHECK` runs `llmr healthcheck`, which asks this path
over loopback.
