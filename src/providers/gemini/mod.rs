//! Google's Gemini models.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | [`crate::Reach::FirstPartyApi`] | The `generateContent` API |
//! | `embed` | [`crate::Reach::FirstPartyApi`] | The `batchEmbedContents` API |
//! | `media` | [`crate::Reach::FirstPartyApi`] | Pictures and speech from `generateContent` |
//!
//! One reach so far. The reason this directory exists rather than a single file is that a
//! caller comparing two reaches for one vendor is what the grouping is for.
//!
//! Gemini models are also served by cloud partners. Those live under the partner rather than
//! here, because a prompt sent through one goes to that company on that credential — see
//! `docs/DESIGN.md` on what the top level of `providers::` names.

#[cfg(feature = "gemini")]
pub mod api;

// Embeddings are a different trait, so they are a sibling of `api` rather than something
// inside it. This is the one reach in the crate where `Purpose` reaches a wire.
#[cfg(all(feature = "gemini", feature = "embeddings"))]
pub mod embed;

// Pictures and speech, which this API answers from `generateContent` asked for a different
// kind of reply.
#[cfg(all(
    feature = "gemini",
    any(feature = "image-generation", feature = "audio")
))]
pub mod media;
