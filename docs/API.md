# Client API

llmr speaks the OpenAI shape (chat completions, embeddings, image generation, speech and
transcription), so any OpenAI SDK or framework works by changing its base URL to
`http://<host>:8080/v1`. This page is what it accepts, what it
refuses, and what it sends back. Setting up what it serves is the
[management API](MANAGEMENT.md).

| Method | Path | Token | |
|---|---|---|---|
| `POST` | `/v1/chat/completions` | when set | A chat call, whole or streamed |
| `POST` | [`/v1/embeddings`](#post-v1embeddings) | when set | Text as vectors |
| `POST` | [`/v1/images/generations`](#post-v1imagesgenerations) | when set | Pictures from a prompt |
| `POST` | [`/v1/audio/speech`](#post-v1audiospeech) | when set | Text read aloud |
| `POST` | [`/v1/audio/transcriptions`](#post-v1audiotranscriptions) | when set | A recording written down |
| `GET` | `/v1/models` | when set | The names a client can use |
| `GET` | `/healthz` | never | Liveness: `200 ok` while the process serves |

With `LLMR_TOKEN` set, a call presents one of its tokens as `Authorization: Bearer <token>`
or `x-api-key: <token>`. OpenAI SDKs send their `api_key` as the first, so setting the SDK's
key to the token is all it takes. Without `LLMR_TOKEN`, nothing is checked.

## `POST /v1/chat/completions`

### `model`

A route set, such as `default`, or an enabled model addressed directly as `provider/model`,
such as `anthropic/claude-haiku-4-5`. A model that exists and is not enabled is a
`404 model_not_enabled` saying so; any other name is a `404 model_not_found`. A route set or
model of another kind, such as an embedding model, is a `400 wrong_endpoint` naming the
endpoint it belongs to.

The reply's `model` is the model that actually answered, not the name asked for.

### What is carried

| Field | Notes |
|---|---|
| `messages` | `system`, `developer`, `user`, `assistant` and `tool` roles. `system` and `developer` text becomes the system prompt. Content as a string or as `text` parts |
| images | `image_url` parts in a user message, as a `data:` URL or an `http(s)` link ending in `.png`, `.jpg`, `.jpeg`, `.gif` or `.webp` |
| documents | `file` parts in a user message: `{"type": "file", "file": {"file_data": "data:application/pdf;base64,...", "filename": "report.pdf"}}`. `file_data` is a base64 `data:` URL, or bare base64 when `filename` ends in `.pdf` or `.txt`, which gives the type |
| audio | `input_audio` parts in a user message: `{"type": "input_audio", "input_audio": {"data": "<base64>", "format": "wav"}}`, with `format` one of `wav`, `mp3`, `flac`, `ogg`, `aac`, `m4a`, `webm` |
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
- `audio`, `modalities` (audio in the reply), `prediction`, `web_search_options`, the legacy
  `functions`
- an image link whose type cannot be read from its extension; send a data URL
- a `file` part with `file_id`: llmr has no files endpoint, so send the file as `file_data`
- a `file` part whose bare base64 `file_data` has no `.pdf` or `.txt` name to type it
- an `input_audio` part in any other format
- a content part other than text, an image, a file or input audio

### Choosing a route

The request is matched against what each route can do. A request with tools skips routes
that cannot take tools; one with an image, a document or audio skips routes without the
`images`, `documents` or `audio` capability; one with a schema or a `reasoning_effort` other
than `none` skips routes without structured output or reasoning. Among the routes that fit,
they are tried in the route set's order until one answers.

What each provider takes beyond the capability:

| Provider | Documents | Audio |
|---|---|---|
| `anthropic` | PDF, and plain text (sent as text) | none: a request with audio fails this route |
| `openai`, `openai-compatible` | PDF only | `wav` and `mp3` only |
| `gemini` | inline, with the type the client gave | inline, with the type the client gave |

A document or recording a route's provider does not take fails that route before anything
is sent, and the next route is tried.

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

## Endpoints beside chat

Embeddings, image generation, speech and transcription each have their own endpoint in the
OpenAI shape, and are served by models of the matching kind: a model is `chat`,
`embedding`, `image`, `speech` or `transcription`, set when it is enabled
([management API](MANAGEMENT.md#kinds)). A route set serves one kind.

What they share:

- **`model`** is a route set or an enabled `provider/model`, as for chat. A name of another
  kind is a `400 wrong_endpoint` naming the endpoint it belongs to; the same is true of an
  embedding model sent to `/v1/chat/completions`.
- **Routes are tried in the order listed.** Each gets the set's `retry_attempts`, and only a
  rate limit, a timeout or a transient failure is retried. `deadline_secs` bounds the whole
  request. `on_device`, and the `x-llmr-on-device` header, keep it on `self-hosted` routes as
  in chat. The set's `order` and `breaker` apply to chat only: here there is no reordering by
  price or health, and a failing route is not rested. A `provider/model` asked for directly
  gets two attempts and no deadline.
- **What a provider cannot honour fails that route before the call**, and the next route is
  tried: a size Gemini cannot draw, a speech format it does not produce. When every route
  has failed, the answer is the last failure, for example `400 no_route` with the reason.
- **A refusal stops**, as in chat: the next route is not asked. These shapes have no way to
  write a refusal into a reply, so it is a `422 refused`.
- **A field that cannot be carried is refused by name**, never dropped.
- Every answer carries `x-llmr-route`, `x-llmr-attempts`, `x-llmr-fell-through`,
  `x-llmr-request-id` (the id its usage row is recorded under) and, when priced,
  `x-llmr-cost`. A JSON reply also carries `llmr_cost`.
- **Cost** comes from the vendor's price book when it has a row for the model that
  answered. The shipped price books list chat models only, so a vendor's embedding, image,
  speech and transcription models are `unpriced`; a `self-hosted` route is `free`.
- Each request is recorded in [usage](MANAGEMENT.md#usage) like a chat call, with the route,
  attempts, the tokens the provider reported, and the cost.

Which provider type serves which kind:

| Kind | `openai`, `openai-compatible` | `gemini` | `anthropic` |
|---|---|---|---|
| `embedding` | `/embeddings` | `batchEmbedContents` | no |
| `image` | `/images/generations` | `generateContent` | no |
| `speech` | `/audio/speech` | `generateContent` | no |
| `transcription` | `/audio/transcriptions` | no: send the recording to a chat model that takes `audio` | no |

## `POST /v1/embeddings`

```json
{ "model": "vectors", "input": ["first text", "second text"] }
```

| Field | Notes |
|---|---|
| `model` | Required. A route set or `provider/model` of kind `embedding` |
| `input` | Required. A string, or a non-empty array of strings. Token arrays are refused: they are one tokenizer's numbers and mean nothing to another vendor's model |
| `encoding_format` | `float` (default) or `base64`, the little endian 32 bit floats, which is what the OpenAI SDKs ask for by default |
| `dimensions` | A positive integer, for models that can shorten their vectors |

```json
{
  "object": "list",
  "data": [
    { "object": "embedding", "index": 0, "embedding": [0.0123, -0.0456, "..."] },
    { "object": "embedding", "index": 1, "embedding": [0.0789, 0.0012, "..."] }
  ],
  "model": "text-embedding-3-small",
  "usage": { "prompt_tokens": 8, "total_tokens": 8 },
  "llmr_cost": { "status": "unpriced" }
}
```

`data` is in the order of `input`. `usage` is left out when the provider reported no count,
which Gemini's embeddings API never does, rather than written as zero.

## `POST /v1/images/generations`

```json
{ "model": "pictures", "prompt": "a lighthouse at dusk, watercolour", "size": "1024x1024" }
```

| Field | Notes |
|---|---|
| `model` | Required. Kind `image` |
| `prompt` | Required, not blank |
| `n` | 1 to 10 |
| `size`, `quality`, `background`, `output_format` | Passed on when given |
| `response_format` | `url` or `b64_json`. Sent only when given, so each model answers the way it always does: GPT image models with bytes, DALL·E models with a link |

Refused by name: `stream` and `partial_images` (a picture comes back whole), `style` (say it
in the prompt), `moderation`, `output_compression`.

```json
{
  "created": 1790467200,
  "data": [ { "b64_json": "iVBORw0KGgo...", "revised_prompt": "..." } ],
  "output_format": "png",
  "usage": { "input_tokens": 14, "output_tokens": 4160, "total_tokens": 4174 },
  "llmr_cost": { "status": "unpriced" }
}
```

Each picture is `b64_json` or `url`, whichever the provider answered with. `revised_prompt` is
on the first picture when the provider rewrote the prompt. `output_format` is the format of
the first picture that came as bytes, and `usage` is present only when the provider
reported it.

**Gemini** draws through `generateContent`, which has less to say:

- one picture per call: `n` other than 1 fails the route;
- `size` becomes an aspect ratio, and must reduce to one of `1:1`, `2:3`, `3:2`, `3:4`,
  `4:3`, `4:5`, `5:4`, `9:16`, `16:9`, `21:9` (or be `auto`);
- no `quality`, `background` or `output_format`;
- bytes only: `response_format` other than `b64_json` fails the route;
- a prompt its safety filters block is a refusal, a `422 refused`.

Imagen models, which Google serves through `:predict` rather than `generateContent`, are not
supported.

## `POST /v1/audio/speech`

```json
{ "model": "voice", "input": "Your order has shipped.", "voice": "alloy", "response_format": "mp3" }
```

| Field | Notes |
|---|---|
| `model` | Required. Kind `speech` |
| `input` | Required, not blank |
| `voice` | Required. A name, or an object with an `id` |
| `response_format` | See below |
| `speed` | 0.25 to 4.0 |
| `instructions` | How to read it |
| `stream_format` | `audio` only; `sse` is refused |

The reply is the recording itself, with its `Content-Type`, and the `x-llmr-*` headers.
There is no JSON body, so the cost is in `x-llmr-cost` only.

| Provider | `response_format` | Notes |
|---|---|---|
| `openai`, `openai-compatible` | `mp3` (default), `opus`, `aac`, `flac`, `wav`, `pcm` | `pcm` is raw 24 kHz 16 bit mono samples, typed `audio/pcm` |
| `gemini` | `wav` (default) or `pcm` | No `speed` other than 1. `voice` is one of Gemini's prebuilt voice names. `instructions` are put in front of the text (`"<instructions>: <input>"`), which is how these models are directed. Raw samples are wrapped as WAV |

## `POST /v1/audio/transcriptions`

A `multipart/form-data` upload, as the OpenAI SDKs send it:

```sh
curl localhost:8080/v1/audio/transcriptions \
  -F model=openai/gpt-4o-transcribe -F file=@meeting.m4a -F language=en
```

| Field | Notes |
|---|---|
| `model` | Required. Kind `transcription` |
| `file` | Required. Its type is the part's `Content-Type` when that is `audio/*` or `video/*`, otherwise read from the name: `.mp3`, `.mpga`, `.mpeg`, `.wav`, `.flac`, `.ogg`, `.webm`, `.m4a`, `.mp4` |
| `language`, `prompt` | Passed on |
| `temperature` | 0 to 1 |
| `response_format` | `json` (default) or `text` |

Refused by name: `response_format` `srt`, `vtt` or `verbose_json` (most transcription models
cannot produce timings, and a caller who needs them should know before the call),
`timestamp_granularities[]`, `include[]`, `stream`, `chunking_strategy`.

```json
{ "text": "Thanks everyone for joining.", "usage": { "type": "duration", "seconds": 42.0 },
  "llmr_cost": { "status": "unpriced" } }
```

`usage` is token counts (`input_tokens`, `output_tokens`, `total_tokens`) when the provider
reported them, or `{"type": "duration", "seconds": ...}` when it reported the length instead,
and absent otherwise. With `response_format: "text"` the body is the text alone, as
`text/plain`, and the cost is in `x-llmr-cost`.

Only `openai` and `openai-compatible` providers serve transcription. Gemini has no
transcription endpoint: send the recording to `/v1/chat/completions` as `input_audio`, to a
model with the `audio` capability, and ask for a transcript.

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
| 400 | `wrong_endpoint` | The name is a route set or model of another kind; the message names the endpoint it belongs to |
| 401 | `invalid_api_key` | `LLMR_TOKEN` is set and the call did not present one of its tokens |
| 404 | `model_not_found` | Not a route set or a known model |
| 404 | `model_not_enabled` | A model that exists and is switched off |
| 404 | `not_found` | No such endpoint |
| 422 | `refused` | The model declined, on an endpoint beside chat. A chat refusal is a `200`, as above |
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
  { "id": "default", "object": "model", "created": 0, "owned_by": "llmr", "llmr_kind": "chat" },
  { "id": "vectors", "object": "model", "created": 0, "owned_by": "llmr", "llmr_kind": "embedding" },
  { "id": "anthropic/claude-sonnet-5", "object": "model", "created": 0, "owned_by": "llmr", "llmr_kind": "chat" }
] }
```

Route sets first, because those are the names a client is meant to use, then every enabled
model as `provider/model`. `llmr_kind` says which endpoint a name belongs to: `chat`,
`embedding`, `image`, `speech` or `transcription`. What each route set is made of, and which
of its routes are usable or resting, is `GET /manage/routes`.

## `GET /healthz`

`200 ok`, no key. The image's `HEALTHCHECK` runs `llmr healthcheck`, which asks this path
over loopback.
