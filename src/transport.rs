//! The HTTP boundary, and one implementation of it.
//!
//! Every provider that speaks to a network endpoint goes through [`HttpTransport`]. It
//! exists so a provider can be tested without a server: the tests in this crate hand a
//! recorded reply to the same code that talks to the vendor, so what they check is the
//! request that would go on the wire.

use crate::{Error, Result};
use async_trait::async_trait;
use futures_core::Stream;
use std::pin::Pin;
use std::time::Duration;

/// A reply arriving in pieces.
///
/// Chunks as the network hands them over, with no promise about where the boundaries fall.
/// A chunk is not a line and not a frame: reassembling those is the caller's job, which for
/// this crate means [`crate::providers::api::ApiProvider`].
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Vec<u8>>> + Send + 'static>>;

/// Which HTTP verb.
///
/// Two, because two is what these protocols use. A chat is a POST and a model list is a
/// GET, and a transport that assumed one of them would make the other work by accident on
/// some servers and fail on others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Read.
    Get,
    /// Send a body.
    Post,
}

impl Method {
    /// The verb, as it goes on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
        }
    }
}

/// One HTTP request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpRequest {
    /// Which verb.
    pub method: Method,
    /// Where to send it.
    pub url: String,
    /// Header names and values, in the order they should be sent.
    pub headers: Vec<(String, String)>,
    /// The body, already serialized.
    pub body: Vec<u8>,
}

impl HttpRequest {
    /// A POST with a body and no headers.
    pub fn new(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            method: Method::Post,
            url: url.into(),
            headers: Vec::new(),
            body,
        }
    }

    /// A GET with no headers.
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            method: Method::Get,
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Adds a header.
    #[must_use]
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// One HTTP reply.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The body.
    pub body: Vec<u8>,
    /// How long the server asked you to wait, when it sent `Retry-After`.
    pub retry_after: Option<Duration>,
}

impl HttpResponse {
    /// A reply with a status and a body.
    ///
    /// These types are marked non exhaustive so fields can be added without breaking your
    /// code, which also means you cannot build one with a struct literal. This is how you
    /// build one, and it is what an implementation of [`HttpTransport`] needs.
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            retry_after: None,
        }
    }

    /// Records what the server asked you to wait, from its `Retry-After` header.
    #[must_use]
    pub fn with_retry_after(mut self, wait: Duration) -> Self {
        self.retry_after = Some(wait);
        self
    }

    /// Turns a status code into the error it means.
    ///
    /// # Errors
    ///
    /// Returns `Ok(())` for a 2xx and an [`Error`] for everything else. The mapping is
    /// here rather than in each provider so that two providers cannot disagree about what
    /// a 429 means.
    pub fn check(&self) -> Result<()> {
        let body = String::from_utf8_lossy(&self.body);
        let detail = body.chars().take(400).collect::<String>();
        match self.status {
            200..=299 => Ok(()),
            401 | 403 => Err(Error::Auth(detail)),
            404 => Err(Error::NotFound(detail)),
            408 => Err(Error::Timeout {
                elapsed: Duration::ZERO,
            }),
            429 => Err(Error::RateLimited {
                retry_after: self.retry_after,
            }),
            // 400 is the caller's request, and retrying it produces the same 400.
            400 | 422 => Err(Error::InvalidRequest(detail)),
            // A server side fault is worth trying again. So is a gateway that gave up.
            500..=599 => Err(Error::Transient(format!("{}: {detail}", self.status))),
            other => Err(Error::Transient(format!("{other}: {detail}"))),
        }
    }
}

/// Something that can send an HTTP request.
///
/// Implementations must be safe to share. `send` takes `&self` and nothing in this crate
/// holds a lock across it.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Sends one request.
    ///
    /// # Errors
    ///
    /// A transport error, such as a connection that could not be made. A reply with a
    /// failing status code is still `Ok`, because reading the status is
    /// [`HttpResponse::check`]'s job and a provider may want the body first.
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse>;

    /// Sends one request and hands back the reply as it arrives.
    ///
    /// The default sends it the ordinary way and yields the whole body as a single chunk,
    /// so a transport written before this method existed still compiles and still works.
    /// What it costs is the streaming: everything arrives at once, at the end.
    ///
    /// Unlike [`HttpTransport::send`], the status is checked here, before any bytes are
    /// handed over. A caller reading a stream has nowhere to put a 429 once the first chunk
    /// has already been consumed as content.
    ///
    /// # Errors
    ///
    /// A transport error, or whatever [`HttpResponse::check`] makes of a failing status. A
    /// failure *after* the first chunk arrives is an `Err` item inside the stream instead.
    async fn send_streaming(&self, request: HttpRequest) -> Result<ByteStream> {
        let response = self.send(request).await?;
        response.check()?;
        Ok(Box::pin(Whole {
            chunk: Some(response.body),
        }))
    }
}

/// A body that was never really streamed, as a stream of one chunk.
struct Whole {
    chunk: Option<Vec<u8>>,
}

impl Stream for Whole {
    type Item = Result<Vec<u8>>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::task::Poll::Ready(self.chunk.take().map(Ok))
    }
}

/// An [`HttpTransport`] backed by `reqwest`.
///
/// Holds one client, which `reqwest` documents as cheap to clone and safe to share. There
/// is no lock in here, so any number of calls can be in flight at once.
#[cfg(feature = "reqwest")]
#[cfg_attr(docsrs, doc(cfg(feature = "reqwest")))]
#[derive(Debug, Clone)]
pub struct Reqwest {
    client: reqwest::Client,
}

#[cfg(feature = "reqwest")]
impl Reqwest {
    /// A transport with a request timeout.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Transient`] when the underlying client cannot be built, which in
    /// practice means the TLS backend failed to start.
    pub fn new(timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| Error::Transient(format!("http client: {e}")))?;
        Ok(Self { client })
    }

    /// Wraps a client you built yourself, so proxy and TLS settings are yours to choose.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

#[cfg(feature = "reqwest")]
#[async_trait]
impl HttpTransport for Reqwest {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse> {
        let mut builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url).body(request.body),
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                Error::Timeout {
                    elapsed: Duration::ZERO,
                }
            } else {
                Error::Transient(e.to_string())
            }
        })?;

        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        let body = response
            .bytes()
            .await
            .map_err(|e| Error::Transient(format!("reading the body: {e}")))?
            .to_vec();

        Ok(HttpResponse {
            status,
            body,
            retry_after,
        })
    }

    async fn send_streaming(&self, request: HttpRequest) -> Result<ByteStream> {
        let mut builder = match request.method {
            Method::Get => self.client.get(&request.url),
            Method::Post => self.client.post(&request.url).body(request.body),
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }

        let response = builder.send().await.map_err(|e| {
            if e.is_timeout() {
                Error::Timeout {
                    elapsed: Duration::ZERO,
                }
            } else {
                Error::Transient(e.to_string())
            }
        })?;

        // The status, before a single byte is handed on. Once a caller is reading chunks as
        // content there is nowhere left to put a 429, and the body of an error reply would
        // arrive looking like an answer.
        let status = response.status().as_u16();
        if !(200..=299).contains(&status) {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(Duration::from_secs);
            let body = response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .unwrap_or_default();
            return Err(
                match (HttpResponse {
                    status,
                    body,
                    retry_after,
                })
                .check()
                {
                    Err(e) => e,
                    // Unreachable: `check` returns `Ok` only for a 2xx, and this branch is the
                    // one where the status was not. Written out rather than unwrapped because
                    // the crate does not panic, not even where it cannot happen.
                    Ok(()) => Error::Transient(format!("{status}: not a success and not an error")),
                },
            );
        }

        Ok(Box::pin(Chunks {
            inner: Box::pin(response.bytes_stream()),
        }))
    }
}

/// `reqwest`'s byte stream, with its error type mapped to this crate's.
///
/// Generic over the chunk type so this file never has to name `bytes::Bytes`, which would
/// mean depending on `bytes` directly to say one word.
#[cfg(feature = "reqwest")]
struct Chunks<S> {
    inner: Pin<Box<S>>,
}

#[cfg(feature = "reqwest")]
impl<S, B> Stream for Chunks<S>
where
    S: Stream<Item = reqwest::Result<B>>,
    B: AsRef<[u8]>,
{
    type Item = Result<Vec<u8>>;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().inner.as_mut().poll_next(cx).map(|chunk| {
            chunk.map(|chunk| {
                chunk
                    .map(|bytes| bytes.as_ref().to_vec())
                    // A stream that breaks halfway is transient rather than unreadable: the
                    // bytes that arrived were fine, the connection was not.
                    .map_err(|e| Error::Transient(format!("reading the stream: {e}")))
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(status: u16) -> HttpResponse {
        HttpResponse {
            status,
            body: b"something went wrong".to_vec(),
            retry_after: None,
        }
    }

    #[test]
    fn a_bad_credential_is_never_retried() {
        assert!(!reply(401)
            .check()
            .err()
            .map(|e| e.is_retryable())
            .unwrap_or(true));
    }

    #[test]
    fn a_malformed_request_is_not_retried_either() {
        // Sending the same 400 again produces the same 400.
        let err = reply(400).check().err();
        assert!(matches!(err, Some(Error::InvalidRequest(_))));
    }

    #[test]
    fn a_server_fault_is_worth_another_go() {
        assert_eq!(
            reply(503).check().err().map(|e| e.is_retryable()),
            Some(true)
        );
    }

    #[test]
    fn a_rate_limit_carries_the_wait_the_server_asked_for() {
        let limited = HttpResponse {
            status: 429,
            body: Vec::new(),
            retry_after: Some(Duration::from_secs(12)),
        };
        assert_eq!(
            limited.check().err().and_then(|e| e.retry_after()),
            Some(Duration::from_secs(12))
        );
    }

    #[test]
    fn a_success_is_not_an_error() {
        assert!(reply(200).check().is_ok());
        assert!(reply(204).check().is_ok());
    }
}
