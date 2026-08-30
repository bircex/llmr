# Calling Bedrock

`providers::bedrock` ships a translation and no way to authenticate one, on purpose:
`docs/DESIGN.md` has the argument, and the short version is that SigV4 signs the whole HTTP
request and needs a clock, a region and rotating credentials, none of which a pure `Protocol`
may hold. Signing is the transport's job.

That reasoning does not help anybody who enabled the feature and hit a wall. This page is the
worked example beside it. Follow it and you can make a call, without this crate taking on a
signing implementation it should not own.

---

## The shape

Three pieces:

1. **The provider**, from this crate. It builds the URL, writes the body and reads the reply.
2. **A signing transport**, which you write. Twenty lines around whatever HTTP client you
   already have.
3. **A credentials source**, which you almost certainly already have if you are on AWS.

```rust
use llmr::providers::bedrock;
use llmr::{ChatRequest, Message, Provider, Registry, Reach};
use std::sync::Arc;

let region = "eu-west-1";
let transport = Arc::new(SigningTransport::new(region).await?);

let claude = bedrock::api::anthropic_family(
    region,
    transport,
    Arc::new(Registry::empty("bedrock", Reach::CloudPartner)),
);

let reply = claude
    .chat(ChatRequest::new(
        "anthropic.claude-sonnet-5-v1:0",
        vec![Message::user("Hello")],
    ))
    .await?;
```

There is no key argument, and that is not an oversight. A bearer token attached beside a
SigV4 signature is at best ignored and at worst a request Bedrock rejects, so
`InvokeModel::headers` deliberately attaches none.

---

## Where the wrapper goes, and why it is the last thing to touch the request

A signature covers the request **as it will be sent**. So the wrapper has to run after the
protocol has attached its headers and after the URL is final, and nothing may add a signed
header afterwards.

`HttpTransport` is exactly that seam. `ApiProvider` hands it a finished `HttpRequest`, and
what the wrapper does to it is the last thing that happens.

```rust
use llmr::transport::{HttpRequest, HttpResponse, HttpTransport, ByteStream};
use std::sync::Arc;

/// Signs, then hands the request to whatever really sends it.
struct SigningTransport {
    inner: Arc<dyn HttpTransport>,
    region: String,
    credentials: CredentialsSource,
}

#[async_trait::async_trait]
impl HttpTransport for SigningTransport {
    async fn send(&self, request: HttpRequest) -> llmr::Result<HttpResponse> {
        self.inner.send(self.sign(request).await?).await
    }

    async fn send_streaming(&self, request: HttpRequest) -> llmr::Result<ByteStream> {
        self.inner.send_streaming(self.sign(request).await?).await
    }
}
```

Implement both. A wrapper that signs `send` and forwards `send_streaming` unsigned works
until the first streamed call, and then fails with a 403 that says nothing about which of the
two paths was wrong.

---

## What has to be signed, and in what order

SigV4 builds a canonical request, hashes it, signs the hash, and puts the result in an
`authorization` header. Getting any part of it wrong produces the same opaque 403, which is
why the list is worth having in front of you:

| Part | Where it comes from |
|---|---|
| HTTP method | `HttpRequest::method` |
| Canonical URI | the path of `HttpRequest::url` |
| Canonical query string | the query of `HttpRequest::url`, empty for `InvokeModel` |
| Canonical headers | the headers you are going to send, lowercased, sorted by name |
| Signed headers | the names of exactly those headers, in the same order |
| Payload hash | `SHA-256` of `HttpRequest::body`, hex encoded |

Then three headers go back on the request:

* `x-amz-date`, the timestamp the signature covers
* `authorization`, the signature itself
* `x-amz-security-token`, **only** when the credentials are temporary

The order to do it in:

1. Take the request as `ApiProvider` handed it over. Do not add or remove headers after this
   point except the ones below.
2. Add `x-amz-security-token` first, if you have one. It is a signed header, so adding it
   after signing invalidates the signature, and that is the single most common way to get a
   403 out of code that looks right.
3. Compute the payload hash over the body exactly as it will be sent.
4. Sign.
5. Add `x-amz-date` and `authorization`.

**`host` must be signed and must match.** Most clients set `host` from the URL themselves, so
the signer has to be told the same host or the two disagree and the request fails. Whether
you sign `host` explicitly or let the signer derive it from the URL, it has to be the host in
`HttpRequest::url`.

**Watch the colon in the model id.** A Bedrock model is addressed in the path, and its id
contains a colon: `anthropic.claude-sonnet-5-v1:0`, or `eu.anthropic.claude-sonnet-5-v1:0`
for a cross region profile. If your signer percent-encodes that path segment and your HTTP
client does not, or the other way round, the canonical URI the signature covers is not the
one that arrives. That mismatch is a 403 with no useful message in it, and it is the first
thing to check when a call fails and everything else looks right. Sign the same URL string
your client will send.

---

## Where the region comes from

One region, in two places, and they have to be the same one:

* the endpoint, which `bedrock::api::endpoint` builds as
  `https://bedrock-runtime.{region}.amazonaws.com`
* the credential scope in the signature

`bedrock::api::anthropic_family` takes the region and builds the endpoint from it. Your
transport takes the same string. Pass one value to both rather than reading it twice from the
environment, so a machine with a stale `AWS_REGION` cannot end up signing for one region and
calling another.

The service name in the scope is `bedrock`, not `bedrock-runtime`, whatever the hostname
says.

A cross region inference profile changes the model id and not the endpoint: an
`eu.`-prefixed model on a `eu-west-1` endpoint is still signed for `eu-west-1`.

---

## Credentials that rotate, and why they live in the transport

An instance role, an assumed role, or SSO gives you credentials with an expiry. Something has
to fetch fresh ones before they lapse, which means the credential holder needs a clock, a
cache, and usually an HTTP call of its own.

That is the argument for signing living in the transport rather than in `Protocol`. Every
`Protocol` method here is a pure function, which is what makes one instance safe to share
across any number of concurrent calls. A protocol that held a credential store with a
refresh timer inside it would end that for every protocol in the crate, to serve one.

A transport has no such constraint. It already holds a connection pool. So:

```rust
impl SigningTransport {
    async fn sign(&self, request: HttpRequest) -> llmr::Result<HttpRequest> {
        // Resolved per call. The provider is built once and lives for the process, and
        // credentials that were fresh at startup are not fresh at hour three.
        let credentials = self.credentials.resolve().await?;
        // ... build the canonical request, sign, attach the headers ...
        Ok(request)
    }
}
```

Resolve per call rather than at construction. A cached credential provider makes that cheap,
and the alternative is a provider that works all afternoon and starts failing at whatever
hour the token expires.

---

## What to reach for

* `aws-sigv4` does the signing. It takes the method, URI, headers and body, and hands back
  headers to attach. It is the crate to use rather than writing one: SigV4 is not hard and it
  is very easy to get subtly wrong, and this is not a thing worth debugging.
* `aws-config` resolves credentials from the whole chain, environment, profile, instance role
  and SSO, and caches them with their expiry.
* `aws-credential-types` is the credential type the two agree on.

This crate depends on none of them, and should not. Writing a SigV4 here would mean a crypto
dependency and an implementation nobody could test against the real thing from inside this
repository. The crate already makes this bargain for HTTP itself: `reqwest` is a feature, not
a requirement.

---

## The reach is not a choice

Everything through `bedrock` reports `Reach::CloudPartner`, never `FirstPartyApi`. A prompt
sent here goes to Amazon on an Amazon credential, whoever trained the model, and a program
asking where its data may go has to be told that. `Requirements::on_device` will not select
it, and that is correct.

---

## No streaming, and what that means

Bedrock streams over its own binary event framing rather than server sent events, so the
frame reader in `providers::api` cannot read it. `Provider::stream` falls back to one whole
call handed over as a burst: a real answer with the same text and the same usage, arriving
all at once.

Say so in your `Registry`. A row with `streaming = false` means
`Requirements::streaming()` skips this route rather than routing to it and leaving somebody
watching a blank screen for thirty seconds.

---

## When it fails

Nearly every SigV4 failure is a 403 with nothing useful in it. In the order worth checking:

1. A header added after signing. `x-amz-security-token` is the usual one.
2. The path encoding, because of the colon in the model id.
3. `host` signed as one thing and sent as another.
4. The region in the scope not matching the region in the hostname.
5. Expired credentials, if the process has been up a while and this used to work.
6. The body hashed before something changed it. Hash exactly the bytes that go out.

An error that is *not* a 403 is usually real: a `ValidationException` means the body reached
Bedrock and it did not like it, which means the signing works.
