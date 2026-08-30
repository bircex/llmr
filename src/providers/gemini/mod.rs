//! Google's Gemini models.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | [`crate::Reach::FirstPartyApi`] | The `generateContent` API |
//!
//! One reach so far. A Gemini command line tool would land beside it as `gemini::cli`, and
//! the reason this directory exists rather than a single file is that a caller comparing
//! two reaches for one vendor is what the grouping is for.
//!
//! Gemini models are also served by cloud partners. Those live under the partner rather than
//! here, because a prompt sent through one goes to that company on that credential — see
//! `docs/DESIGN.md` on what the top level of `providers::` names.

#[cfg(feature = "gemini")]
#[cfg_attr(docsrs, doc(cfg(feature = "gemini")))]
pub mod api;
