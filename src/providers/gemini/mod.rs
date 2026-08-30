//! Google's Gemini models.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | [`crate::Reach::FirstPartyApi`] | The `generateContent` API |
//! | `embed` | [`crate::Reach::FirstPartyApi`] | The `batchEmbedContents` API |
//!
//! One reach so far. A Gemini command line tool would land beside them as `gemini::cli`, and
//! the reason this directory exists rather than a single file is that a caller comparing
//! two reaches for one vendor is what the grouping is for.
//!
//! Gemini models are also served by cloud partners. Those live under the partner rather than
//! here, because a prompt sent through one goes to that company on that credential — see
//! `docs/DESIGN.md` on what the top level of `providers::` names.

#[cfg(feature = "gemini")]
#[cfg_attr(docsrs, doc(cfg(feature = "gemini")))]
pub mod api;

// Embeddings are a different trait, so they are a sibling of `api` rather than something
// inside it. This is the one reach in the crate where `Purpose` reaches a wire.
#[cfg(all(feature = "gemini", feature = "embeddings"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "gemini", feature = "embeddings"))))]
pub mod embed;
