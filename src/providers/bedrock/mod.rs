//! Amazon Bedrock, and the models several vendors serve through it.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | [`crate::Reach::CloudPartner`] | `InvokeModel`, for the Anthropic model family |
//!
//! # Why this is a top level node and not a folder inside `anthropic`
//!
//! Because the top level of `providers::` names **who you reach and whose credential pays**,
//! and for Bedrock that is Amazon. Claude through Bedrock is not Anthropic answering: a
//! different endpoint, a different credential, a different company holding your prompt.
//! `docs/DESIGN.md` has the argument, including the friendlier arrangement that was rejected
//! and what it would have cost.
//!
//! This is also the first thing to use [`crate::Reach::CloudPartner`] for what it was added
//! for. Every route through here reports that reach rather than `FirstPartyApi`, so a
//! program deciding where a prompt may go gets the true answer.
//!
//! # One family so far
//!
//! Bedrock is one address for many vendors' models, and each family takes a different body.
//! [`api::anthropic_family`] speaks the one Anthropic's models take, which is the Messages
//! shape this crate already translates — and it reuses that translation rather than copying
//! it, so a fix to one is a fix to both. Another family means another constructor beside it.

#[cfg(feature = "bedrock")]
#[cfg_attr(docsrs, doc(cfg(feature = "bedrock")))]
pub mod api;
