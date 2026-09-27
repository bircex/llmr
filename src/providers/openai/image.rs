//! Any endpoint speaking the OpenAI images shape: `POST /images/generations`.
//!
//! A shape rather than a vendor, like [`super::api`]: the base URL is a constructor argument.
//!
//! # Nothing is added that the caller did not ask for
//!
//! The GPT image models always answer with bytes and refuse `response_format` outright,
//! while the DALL·E models answer with a link unless told otherwise. So the field is sent
//! only when the caller set it, and each model answers the way it always does; the reply
//! says which it was.

use crate::cost::usage::Usage;
use crate::error::{Error, Result};
use crate::image::{ImageGenerator, ImageRequest, Images, Picture};
use crate::model::ModelId;
use crate::secret::Secret;
use crate::transport::{HttpRequest, HttpTransport};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Anything speaking the OpenAI images shape.
pub struct OpenAiImages {
    id: &'static str,
    base_url: String,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
}

/// An image generator at a base URL, usually ending in `/v1`.
pub fn at(
    id: &'static str,
    base_url: impl Into<String>,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
) -> OpenAiImages {
    OpenAiImages {
        id,
        base_url: base_url.into().trim_end_matches('/').to_string(),
        transport,
        key,
    }
}

impl OpenAiImages {
    fn body(request: &ImageRequest) -> Value {
        let mut body = json!({
            "model": request.model.as_str(),
            "prompt": request.prompt,
        });
        let fields = [
            ("size", &request.size),
            ("quality", &request.quality),
            ("background", &request.background),
            ("output_format", &request.output_format),
            ("response_format", &request.response_format),
        ];
        for (name, value) in fields {
            if let Some(value) = value {
                body[name] = json!(value);
            }
        }
        if let Some(count) = request.count {
            body["n"] = json!(count);
        }
        body
    }

    fn read(body: &Value, request: &ImageRequest) -> Result<Images> {
        let rows = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Unreadable("the reply had no data array".into()))?;
        // The format the reply names, else the one asked for. Neither is a guess.
        let format = body
            .get("output_format")
            .and_then(Value::as_str)
            .or(request.output_format.as_deref());

        let mut pictures = Vec::with_capacity(rows.len());
        let mut revised = None;
        for (index, row) in rows.iter().enumerate() {
            if revised.is_none() {
                revised = row
                    .get("revised_prompt")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            if let Some(data) = row.get("b64_json").and_then(Value::as_str) {
                use base64::Engine as _;
                let data = base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|_| {
                        Error::Unreadable(format!("the picture at {index} is not base64"))
                    })?;
                pictures.push(Picture::Bytes {
                    media_type: media_type(format, &data),
                    data,
                });
            } else if let Some(url) = row.get("url").and_then(Value::as_str) {
                pictures.push(Picture::Url(url.to_string()));
            } else {
                return Err(Error::Unreadable(format!(
                    "the picture at {index} carried neither bytes nor a link"
                )));
            }
        }
        // Not an empty answer. A caller cannot tell one from a failure.
        if pictures.is_empty() {
            return Err(Error::Unreadable("the reply held no pictures".into()));
        }

        let served = body
            .get("model")
            .and_then(Value::as_str)
            .map_or_else(|| request.model.clone(), ModelId::from);
        Ok(Images::new(pictures, served, read_usage(body.get("usage")))
            .with_revised_prompt(revised))
    }
}

/// The media type of a picture that came back.
///
/// From the format the reply or the request named. When neither did, from the signature at
/// the front of the bytes, which every one of these formats starts with; that is reading
/// what arrived rather than guessing at it.
fn media_type(format: Option<&str>, data: &[u8]) -> String {
    match format {
        Some("png") => return "image/png".into(),
        Some("jpeg" | "jpg") => return "image/jpeg".into(),
        Some("webp") => return "image/webp".into(),
        _ => {}
    }
    if data.starts_with(b"\x89PNG") {
        "image/png".into()
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg".into()
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp".into()
    } else {
        "application/octet-stream".into()
    }
}

/// Token counts, for the models that report them. Absent otherwise, never zero.
fn read_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::absent();
    };
    let field = |name: &str| usage.get(name).and_then(Value::as_u64);
    Usage {
        input_tokens: field("input_tokens"),
        cache_read_tokens: None,
        cache_write_tokens: None,
        output_tokens: field("output_tokens"),
        estimated: false,
    }
}

#[async_trait]
impl ImageGenerator for OpenAiImages {
    fn id(&self) -> &str {
        self.id
    }

    async fn generate(&self, request: ImageRequest) -> Result<Images> {
        if request.prompt.trim().is_empty() {
            return Err(Error::InvalidRequest("there is nothing to draw".into()));
        }
        let key = self
            .key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;
        let http = HttpRequest::new(
            format!("{}/images/generations", self.base_url),
            serde_json::to_vec(&Self::body(&request))
                .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?,
        )
        .with_header("authorization", format!("Bearer {key}"))
        .with_header("content-type", "application/json");

        let response = self.transport.send(http).await?;
        response.check()?;
        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))?;
        Self::read(&body, &request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_was_asked_for_is_sent() {
        let body = OpenAiImages::body(&ImageRequest::new("gpt-image-1", "a cat"));
        assert_eq!(body, json!({ "model": "gpt-image-1", "prompt": "a cat" }));

        let body = OpenAiImages::body(
            &ImageRequest::new("dall-e-3", "a cat")
                .with_count(1)
                .with_size("1024x1024"),
        );
        assert_eq!(body["n"], 1);
        assert_eq!(body["size"], "1024x1024");
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn bytes_and_links_are_both_read_and_the_type_is_what_came_back() {
        let reply = json!({
            "created": 1,
            "output_format": "webp",
            "data": [{ "b64_json": "AAAA", "revised_prompt": "a cat, drawn" }],
            "usage": { "input_tokens": 10, "output_tokens": 200 },
        });
        let images =
            OpenAiImages::read(&reply, &ImageRequest::new("gpt-image-1", "a cat")).unwrap();
        assert_eq!(
            images.pictures,
            vec![Picture::Bytes {
                media_type: "image/webp".into(),
                data: vec![0, 0, 0]
            }]
        );
        assert_eq!(images.revised_prompt.as_deref(), Some("a cat, drawn"));
        assert_eq!(images.usage.output_tokens, Some(200));

        let reply = json!({ "data": [{ "url": "https://example.com/1.png" }] });
        let images = OpenAiImages::read(&reply, &ImageRequest::new("dall-e-3", "a cat")).unwrap();
        assert_eq!(
            images.pictures,
            vec![Picture::Url("https://example.com/1.png".into())]
        );
        assert_eq!(
            images.usage,
            Usage::absent(),
            "nothing reported is not zero"
        );
    }

    #[test]
    fn a_reply_with_no_pictures_is_an_error_rather_than_an_empty_answer() {
        let error = OpenAiImages::read(&json!({ "data": [] }), &ImageRequest::new("m", "p"));
        assert!(matches!(error, Err(Error::Unreadable(_))));
    }

    #[test]
    fn a_picture_with_no_format_named_is_typed_by_its_signature() {
        assert_eq!(media_type(None, b"\x89PNG\r\n"), "image/png");
        assert_eq!(media_type(None, &[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
        assert_eq!(media_type(None, b"????"), "application/octet-stream");
    }
}
