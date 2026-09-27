//! Any endpoint speaking the OpenAI audio shape: `/audio/speech` and `/audio/transcriptions`.
//!
//! A shape rather than a vendor, like [`super::api`]. OpenAI answers it, and so do servers
//! running Whisper or a speech model on your own hardware.

use crate::audio::{
    Speech, SpeechRequest, SpeechSynthesizer, Transcriber, Transcription, TranscriptionRequest,
};
use crate::cost::usage::Usage;
use crate::error::{Error, Result};
use crate::secret::Secret;
use crate::transport::{HttpRequest, HttpTransport};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

/// Anything speaking the OpenAI audio shape.
pub struct OpenAiAudio {
    id: &'static str,
    base_url: String,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
}

/// Speech and transcription at a base URL, usually ending in `/v1`.
pub fn at(
    id: &'static str,
    base_url: impl Into<String>,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
) -> OpenAiAudio {
    OpenAiAudio {
        id,
        base_url: base_url.into().trim_end_matches('/').to_string(),
        transport,
        key,
    }
}

/// The media type of a format this shape answers in.
///
/// This shape answers with the bytes and nothing else, so the type is the one asked for.
/// A format it does not name is refused before the call rather than handed back untyped.
fn media_type(format: &str) -> Result<&'static str> {
    Ok(match format {
        "mp3" => "audio/mpeg",
        "opus" => "audio/ogg",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        // Raw 24 kHz 16 bit mono samples, with no header to say so.
        "pcm" => "audio/pcm",
        other => {
            return Err(Error::Unsupported(format!(
                "speech comes as mp3, opus, aac, flac, wav or pcm, not {other}"
            )))
        }
    })
}

impl OpenAiAudio {
    fn bearer(&self) -> Result<String> {
        self.key
            .expose_str()
            .map(|key| format!("Bearer {key}"))
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))
    }

    fn speech_body(request: &SpeechRequest) -> Value {
        let mut body = json!({
            "model": request.model.as_str(),
            "input": request.input,
            "voice": request.voice,
        });
        if let Some(format) = &request.format {
            body["response_format"] = json!(format);
        }
        if let Some(speed) = request.speed {
            body["speed"] = json!(speed);
        }
        if let Some(instructions) = &request.instructions {
            body["instructions"] = json!(instructions);
        }
        body
    }

    /// A multipart body: the recording as a file, and every other field as text.
    ///
    /// `response_format` is always `json`, the one format every transcription model takes,
    /// so the reply can be read the same way whatever the caller wants written back.
    fn transcription_body(request: &TranscriptionRequest, boundary: &str) -> Vec<u8> {
        let mut body = Vec::with_capacity(request.audio.len() + 512);
        let mut field = |name: &str, value: &str| {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        };
        field("model", request.model.as_str());
        field("response_format", "json");
        if let Some(language) = &request.language {
            field("language", language);
        }
        if let Some(prompt) = &request.prompt {
            field("prompt", prompt);
        }
        if let Some(temperature) = request.temperature {
            field("temperature", &temperature.to_string());
        }
        // Quotes and line breaks in a name would end the header early.
        let name: String = request
            .file_name
            .chars()
            .filter(|c| !matches!(c, '"' | '\r' | '\n' | '\\'))
            .collect();
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {}\r\n\r\n",
                request.media_type
            )
            .as_bytes(),
        );
        body.extend_from_slice(&request.audio);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        body
    }

    fn read_transcription(body: &Value, request: &TranscriptionRequest) -> Result<Transcription> {
        let text = body
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Unreadable("the reply had no text".into()))?;
        let usage = body.get("usage");
        let kind = usage.and_then(|u| u.get("type")).and_then(Value::as_str);
        let field = |name: &str| usage.and_then(|u| u.get(name)).and_then(Value::as_u64);
        let (tokens, seconds) = match kind {
            Some("tokens") => (
                Usage {
                    input_tokens: field("input_tokens"),
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    output_tokens: field("output_tokens"),
                    estimated: false,
                },
                None,
            ),
            Some("duration") => (
                Usage::absent(),
                usage.and_then(|u| u.get("seconds")).and_then(Value::as_f64),
            ),
            _ => (Usage::absent(), None),
        };
        Ok(Transcription::new(text, request.model.clone(), tokens).with_seconds(seconds))
    }
}

/// A boundary no recording is likely to contain, different on every call.
fn boundary() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!(
        "llmr-{nanos:x}-{:x}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[async_trait]
impl SpeechSynthesizer for OpenAiAudio {
    fn id(&self) -> &str {
        self.id
    }

    async fn speak(&self, request: SpeechRequest) -> Result<Speech> {
        if request.input.trim().is_empty() {
            return Err(Error::InvalidRequest("there is nothing to say".into()));
        }
        // mp3 is what this shape answers when nothing is asked for.
        let media_type = media_type(request.format.as_deref().unwrap_or("mp3"))?;
        let http = HttpRequest::new(
            format!("{}/audio/speech", self.base_url),
            serde_json::to_vec(&Self::speech_body(&request))
                .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?,
        )
        .with_header("authorization", self.bearer()?)
        .with_header("content-type", "application/json");

        let response = self.transport.send(http).await?;
        response.check()?;
        if response.body.is_empty() {
            return Err(Error::Unreadable("the reply held no audio".into()));
        }
        // No token counts come back; the call is billed by the character.
        Ok(Speech::new(
            media_type,
            response.body,
            request.model,
            Usage::absent(),
        ))
    }
}

#[async_trait]
impl Transcriber for OpenAiAudio {
    fn id(&self) -> &str {
        self.id
    }

    async fn transcribe(&self, request: TranscriptionRequest) -> Result<Transcription> {
        if request.audio.is_empty() {
            return Err(Error::InvalidRequest("there is no recording".into()));
        }
        let boundary = boundary();
        let http = HttpRequest::new(
            format!("{}/audio/transcriptions", self.base_url),
            Self::transcription_body(&request, &boundary),
        )
        .with_header("authorization", self.bearer()?)
        .with_header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        );

        let response = self.transport.send(http).await?;
        response.check()?;
        let body: Value = serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))?;
        Self::read_transcription(&body, &request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcription_is_sent_as_a_multipart_form_with_the_file_last() {
        let mut request =
            TranscriptionRequest::new("whisper-1", b"RIFF".to_vec(), "a\"b.wav", "audio/wav");
        request.language = Some("tr".into());
        let body = String::from_utf8(OpenAiAudio::transcription_body(&request, "XYZ")).unwrap();
        assert!(body.starts_with(
            "--XYZ\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n"
        ));
        assert!(body.contains("name=\"response_format\"\r\n\r\njson\r\n"));
        assert!(body.contains("name=\"language\"\r\n\r\ntr\r\n"));
        assert!(body.contains("name=\"file\"; filename=\"ab.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n--XYZ--\r\n"));
    }

    #[test]
    fn usage_is_tokens_or_seconds_and_never_zero_when_missing() {
        let request = TranscriptionRequest::new("m", vec![1], "a.wav", "audio/wav");
        let read = OpenAiAudio::read_transcription(
            &json!({ "text": "merhaba", "usage": { "type": "tokens", "input_tokens": 7, "output_tokens": 2 } }),
            &request,
        )
        .unwrap();
        assert_eq!(read.text, "merhaba");
        assert_eq!(read.usage.input_tokens, Some(7));

        let read = OpenAiAudio::read_transcription(
            &json!({ "text": "hi", "usage": { "type": "duration", "seconds": 3.5 } }),
            &request,
        )
        .unwrap();
        assert_eq!(read.seconds, Some(3.5));
        assert_eq!(read.usage, Usage::absent());

        assert!(OpenAiAudio::read_transcription(&json!({}), &request).is_err());
    }

    #[test]
    fn speech_is_typed_by_the_format_asked_for_and_an_unknown_one_is_refused() {
        assert_eq!(media_type("mp3").unwrap(), "audio/mpeg");
        assert_eq!(media_type("wav").unwrap(), "audio/wav");
        assert!(matches!(media_type("midi"), Err(Error::Unsupported(_))));
        let body = OpenAiAudio::speech_body(
            &SpeechRequest::new("tts-1", "merhaba", "alloy").with_format("wav"),
        );
        assert_eq!(
            body,
            json!({ "model": "tts-1", "input": "merhaba", "voice": "alloy", "response_format": "wav" })
        );
    }
}
