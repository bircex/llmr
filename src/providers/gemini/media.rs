//! Pictures and speech from Gemini's `generateContent`, asked for a different kind of reply.
//!
//! Gemini has no separate endpoint for either. The same method that chats answers with an
//! image or a recording when `responseModalities` says so, which is why these share the
//! chat endpoint's base URL, key and usage block.
//!
//! # What this cannot do, said before the call
//!
//! One picture per call, at an aspect ratio rather than a pixel size, as bytes; speech as WAV
//! or raw samples, at normal speed. A request for anything else is
//! [`Error::Unsupported`] before anything is sent, because a picture of the wrong shape or a
//! recording in the wrong format is billed and then useless.

use crate::error::{Error, Result};
use crate::model::ModelId;
use crate::secret::Secret;
use crate::transport::{HttpRequest, HttpTransport};
use serde_json::{json, Value};
use std::sync::Arc;

/// Gemini's image and speech models.
pub struct GeminiMedia {
    base_url: String,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
}

/// Gemini's media models at a base URL, [`super::api::DEFAULT_BASE_URL`] for Google's own.
pub fn at(
    base_url: impl Into<String>,
    transport: Arc<dyn HttpTransport>,
    key: Secret,
) -> GeminiMedia {
    GeminiMedia {
        base_url: base_url.into().trim_end_matches('/').to_string(),
        transport,
        key,
    }
}

/// One piece of inline media from a reply.
struct Inline {
    mime_type: String,
    data: Vec<u8>,
}

impl GeminiMedia {
    async fn generate_content(&self, model: &ModelId, body: &Value) -> Result<Value> {
        let key = self
            .key
            .expose_str()
            .map_err(|_| Error::Auth("the API key is not valid UTF-8".into()))?;
        let http = HttpRequest::new(
            format!(
                "{}/models/{}:generateContent",
                self.base_url,
                model.as_str()
            ),
            serde_json::to_vec(body)
                .map_err(|e| Error::InvalidRequest(format!("building the request: {e}")))?,
        )
        .with_header("x-goog-api-key", key)
        .with_header("content-type", "application/json");
        let response = self.transport.send(http).await?;
        response.check()?;
        serde_json::from_slice(&response.body)
            .map_err(|e| Error::Unreadable(format!("the reply was not JSON: {e}")))
    }
}

/// Every inline part of the first candidate.
///
/// None at all is a refusal when the reply says it blocked the prompt, and unreadable
/// otherwise. Never an empty answer.
fn inline_parts(body: &Value) -> Result<Vec<Inline>> {
    if let Some(reason) = body
        .get("promptFeedback")
        .and_then(|f| f.get("blockReason"))
        .and_then(Value::as_str)
    {
        return Err(Error::Refused {
            category: Some(reason.to_string()),
        });
    }
    let candidate = body.get("candidates").and_then(|c| c.get(0));
    let parts = candidate
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut found = Vec::new();
    for part in parts {
        let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) else {
            continue;
        };
        let mime_type = inline
            .get("mimeType")
            .or_else(|| inline.get("mime_type"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let data = inline
            .get("data")
            .and_then(Value::as_str)
            .unwrap_or_default();
        use base64::Engine as _;
        let data = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| Error::Unreadable("inline data in the reply is not base64".into()))?;
        found.push(Inline { mime_type, data });
    }
    if found.is_empty() {
        let why = candidate
            .and_then(|c| c.get("finishReason"))
            .and_then(Value::as_str)
            .unwrap_or("none given");
        return match why {
            "SAFETY" | "PROHIBITED_CONTENT" | "IMAGE_SAFETY" | "BLOCKLIST" | "SPII" => {
                Err(Error::Refused {
                    category: Some(why.to_string()),
                })
            }
            _ => Err(Error::Unreadable(format!(
                "the reply held no media (finish reason: {why})"
            ))),
        };
    }
    Ok(found)
}

fn served(body: &Value, asked: &ModelId) -> ModelId {
    body.get("modelVersion")
        .and_then(Value::as_str)
        .map_or_else(|| asked.clone(), ModelId::from)
}

#[cfg(feature = "image-generation")]
mod pictures {
    use super::*;
    use crate::image::{ImageGenerator, ImageRequest, Images, Picture};
    use async_trait::async_trait;

    /// The ratios the image models draw at.
    const RATIOS: [&str; 10] = [
        "1:1", "2:3", "3:2", "3:4", "4:3", "4:5", "5:4", "9:16", "16:9", "21:9",
    ];

    /// A pixel size as the ratio this API takes, when it is one of the ratios it draws.
    pub(super) fn ratio(size: &str) -> Result<Option<String>> {
        if size == "auto" {
            return Ok(None);
        }
        let parsed = size
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
            .filter(|(w, h)| *w > 0 && *h > 0);
        let Some((w, h)) = parsed else {
            return Err(Error::InvalidRequest(format!(
                "size {size:?} is not WIDTHxHEIGHT"
            )));
        };
        fn gcd(a: u32, b: u32) -> u32 {
            if b == 0 {
                a
            } else {
                gcd(b, a % b)
            }
        }
        let d = gcd(w, h);
        let ratio = format!("{}:{}", w / d, h / d);
        if RATIOS.contains(&ratio.as_str()) {
            Ok(Some(ratio))
        } else {
            Err(Error::Unsupported(format!(
                "Gemini draws at {} and {size} is {ratio}",
                RATIOS.join(", ")
            )))
        }
    }

    pub(super) fn body(request: &ImageRequest) -> Result<Value> {
        if request.count.is_some_and(|n| n != 1) {
            return Err(Error::Unsupported(
                "Gemini draws one picture per call".into(),
            ));
        }
        if request
            .response_format
            .as_deref()
            .is_some_and(|f| f != "b64_json")
        {
            return Err(Error::Unsupported(
                "Gemini hands back the picture itself, not a link; ask for b64_json".into(),
            ));
        }
        for (name, value) in [
            ("quality", &request.quality),
            ("background", &request.background),
            ("output_format", &request.output_format),
        ] {
            if value.is_some() {
                return Err(Error::Unsupported(format!(
                    "Gemini's image models take no {name}"
                )));
            }
        }
        let mut config = json!({ "responseModalities": ["TEXT", "IMAGE"] });
        if let Some(ratio) = request.size.as_deref().map(ratio).transpose()?.flatten() {
            config["imageConfig"] = json!({ "aspectRatio": ratio });
        }
        Ok(json!({
            "contents": [{ "role": "user", "parts": [{ "text": request.prompt }] }],
            "generationConfig": config,
        }))
    }

    #[async_trait]
    impl ImageGenerator for GeminiMedia {
        fn id(&self) -> &str {
            "gemini-images"
        }

        async fn generate(&self, request: ImageRequest) -> Result<Images> {
            if request.prompt.trim().is_empty() {
                return Err(Error::InvalidRequest("there is nothing to draw".into()));
            }
            let reply = self
                .generate_content(&request.model, &body(&request)?)
                .await?;
            let pictures = inline_parts(&reply)?
                .into_iter()
                .filter(|inline| inline.mime_type.starts_with("image/"))
                .map(|inline| Picture::Bytes {
                    media_type: inline.mime_type,
                    data: inline.data,
                })
                .collect::<Vec<_>>();
            if pictures.is_empty() {
                return Err(Error::Unreadable("the reply held no picture".into()));
            }
            Ok(Images::new(
                pictures,
                served(&reply, &request.model),
                super::super::api::read_usage(reply.get("usageMetadata")),
            ))
        }
    }
}

#[cfg(feature = "audio")]
mod speech {
    use super::*;
    use crate::audio::{Speech, SpeechRequest, SpeechSynthesizer};
    use async_trait::async_trait;

    pub(super) fn body(request: &SpeechRequest) -> Result<Value> {
        if request
            .speed
            .is_some_and(|s| (s - 1.0).abs() > f32::EPSILON)
        {
            return Err(Error::Unsupported(
                "Gemini's speech models take no speed; say how to read it in instructions".into(),
            ));
        }
        // Direction is part of the text for these models: they read "say cheerfully: ..."
        // as how to say what follows.
        let text = match &request.instructions {
            Some(how) => format!("{how}: {}", request.input),
            None => request.input.clone(),
        };
        Ok(json!({
            "contents": [{ "role": "user", "parts": [{ "text": text }] }],
            "generationConfig": {
                "responseModalities": ["AUDIO"],
                "speechConfig": {
                    "voiceConfig": { "prebuiltVoiceConfig": { "voiceName": request.voice } },
                },
            },
        }))
    }

    /// A recording as the format asked for: WAV unless raw samples were wanted.
    ///
    /// Older speech models answer headerless 16 bit samples and say so in the type
    /// (`audio/L16;codec=pcm;rate=24000`); newer ones answer WAV. Either becomes what was
    /// asked for, and anything else is handed on with the type it came with.
    pub(super) fn shape(mime_type: &str, data: Vec<u8>, format: &str) -> Result<(String, Vec<u8>)> {
        let lower = mime_type.to_ascii_lowercase();
        let raw = lower.starts_with("audio/l16") || lower.starts_with("audio/pcm");
        let wav = lower.starts_with("audio/wav") || lower.starts_with("audio/x-wav");
        match (format, raw, wav) {
            ("wav", true, _) => Ok(("audio/wav".into(), wav_of(&data, rate(&lower)))),
            ("pcm", true, _) => Ok(("audio/pcm".into(), data)),
            ("wav", _, true) => Ok(("audio/wav".into(), data)),
            ("pcm", _, true) => Ok(("audio/pcm".into(), samples_of(&data)?)),
            _ => Err(Error::Unreadable(format!(
                "the speech came back as {mime_type}, which is neither samples nor WAV"
            ))),
        }
    }

    fn rate(mime_type: &str) -> u32 {
        mime_type
            .split(';')
            .filter_map(|p| p.trim().strip_prefix("rate="))
            .find_map(|r| r.parse().ok())
            .unwrap_or(24_000)
    }

    /// 16 bit mono samples, with the 44 byte header that makes them a WAV file.
    pub(super) fn wav_of(samples: &[u8], rate: u32) -> Vec<u8> {
        let len = u32::try_from(samples.len()).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(samples.len() + 44);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&len.saturating_add(36).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&1u16.to_le_bytes()); // mono
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&rate.saturating_mul(2).to_le_bytes()); // bytes per second
        out.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
        out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        out.extend_from_slice(b"data");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(samples);
        out
    }

    /// The samples inside a WAV file: whatever follows its `data` chunk header.
    fn samples_of(wav: &[u8]) -> Result<Vec<u8>> {
        let mut at = 12;
        while at + 8 <= wav.len() {
            let id = &wav[at..at + 4];
            let size = u32::from_le_bytes([wav[at + 4], wav[at + 5], wav[at + 6], wav[at + 7]]);
            let size = usize::try_from(size).unwrap_or(usize::MAX);
            if id == b"data" {
                let end = at.saturating_add(8).saturating_add(size).min(wav.len());
                return Ok(wav[at + 8..end].to_vec());
            }
            at = at.saturating_add(8).saturating_add(size);
        }
        Err(Error::Unreadable("the WAV reply had no data chunk".into()))
    }

    #[async_trait]
    impl SpeechSynthesizer for GeminiMedia {
        fn id(&self) -> &str {
            "gemini-speech"
        }

        async fn speak(&self, request: SpeechRequest) -> Result<Speech> {
            if request.input.trim().is_empty() {
                return Err(Error::InvalidRequest("there is nothing to say".into()));
            }
            // WAV is what a caller that asked for nothing gets: the one format both kinds of
            // reply can become without re-encoding.
            let format = request.format.as_deref().unwrap_or("wav");
            if !matches!(format, "wav" | "pcm") {
                return Err(Error::Unsupported(format!(
                    "Gemini's speech comes as wav or pcm, not {format}"
                )));
            }
            let reply = self
                .generate_content(&request.model, &body(&request)?)
                .await?;
            let inline = inline_parts(&reply)?
                .into_iter()
                .find(|inline| inline.mime_type.to_ascii_lowercase().starts_with("audio/"))
                .ok_or_else(|| Error::Unreadable("the reply held no audio".into()))?;
            let (media_type, data) = shape(&inline.mime_type, inline.data, format)?;
            Ok(Speech::new(
                media_type,
                data,
                served(&reply, &request.model),
                super::super::api::read_usage(reply.get("usageMetadata")),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_blocked_prompt_is_a_refusal_and_an_empty_reply_is_unreadable() {
        let blocked = json!({ "promptFeedback": { "blockReason": "SAFETY" } });
        assert!(matches!(inline_parts(&blocked), Err(Error::Refused { .. })));
        let empty = json!({ "candidates": [{ "content": { "parts": [{ "text": "no" }] }, "finishReason": "STOP" }] });
        assert!(matches!(inline_parts(&empty), Err(Error::Unreadable(_))));
    }

    #[cfg(feature = "image-generation")]
    #[test]
    fn a_size_becomes_a_ratio_and_what_cannot_be_drawn_is_refused() {
        use crate::image::ImageRequest;
        assert_eq!(
            pictures::ratio("1024x1024").unwrap().as_deref(),
            Some("1:1")
        );
        assert_eq!(
            pictures::ratio("1536x1024").unwrap().as_deref(),
            Some("3:2")
        );
        assert_eq!(pictures::ratio("auto").unwrap(), None);
        assert!(matches!(
            pictures::ratio("1000x333"),
            Err(Error::Unsupported(_))
        ));
        assert!(matches!(
            pictures::ratio("big"),
            Err(Error::InvalidRequest(_))
        ));

        let body = pictures::body(
            &ImageRequest::new("gemini-2.5-flash-image", "a cat").with_size("1792x1024"),
        )
        .err();
        assert!(
            matches!(body, Some(Error::Unsupported(_))),
            "7:4 is not a ratio it draws"
        );
        assert!(pictures::body(&ImageRequest::new("m", "a cat").with_count(2)).is_err());
        let body = pictures::body(&ImageRequest::new("m", "a cat").with_size("1024x1024")).unwrap();
        assert_eq!(
            body["generationConfig"]["imageConfig"]["aspectRatio"],
            "1:1"
        );
    }

    #[cfg(feature = "audio")]
    #[test]
    fn raw_samples_become_a_wav_file_and_a_wav_file_gives_up_its_samples() {
        let (media_type, wav) =
            speech::shape("audio/L16;codec=pcm;rate=24000", vec![1, 2, 3, 4], "wav").unwrap();
        assert_eq!(media_type, "audio/wav");
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            24_000
        );
        assert_eq!(&wav[44..], &[1, 2, 3, 4]);

        let (media_type, samples) = speech::shape("audio/wav", wav, "pcm").unwrap();
        assert_eq!(media_type, "audio/pcm");
        assert_eq!(samples, vec![1, 2, 3, 4]);
    }
}
