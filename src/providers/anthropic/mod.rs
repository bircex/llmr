//! Anthropic, by every reach this crate has.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | [`crate::Reach::FirstPartyApi`] | The Messages API |
//!
//! Anthropic's models are also served by gateways — Bedrock among them — and those live
//! under the gateway rather than here, because a prompt sent through one goes to that
//! company on that credential. See [`crate::Reach::CloudPartner`], and `docs/DESIGN.md` for
//! why the tree is arranged that way.
//!
//! Anthropic direct and a gateway are not fallbacks for each other by default. Ask
//! [`crate::Provider::capabilities`] which one can carry your request, or give both to a
//! [`crate::Router`] and let it read the answer for you.

#[cfg(feature = "anthropic")]
pub mod api;
