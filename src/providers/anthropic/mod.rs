//! Anthropic, by every reach this crate has.
//!
//! | Module | Reach | What it is |
//! |---|---|---|
//! | `api` | [`crate::Reach::FirstPartyApi`] | The Messages API |
//! | `cli` | [`crate::Reach::LocalCli`] | The Claude Code tool on this machine |
//!
//! The same models answer through both, and they are not interchangeable. The API takes
//! tools, a response schema and cache breakpoints, and reports its token counts. The command
//! line tool takes none of those and, on a subscription, reports nothing at all — so its
//! usage comes back [absent rather than zero](crate::Usage::absent), and a cost report that
//! adds it up says what it does not know.
//!
//! Neither is a fallback for the other by default. Ask [`crate::Provider::capabilities`]
//! which one can carry your request, or give both to a [`crate::Router`] and let it read the
//! answer for you.

#[cfg(feature = "anthropic")]
#[cfg_attr(docsrs, doc(cfg(feature = "anthropic")))]
pub mod api;

#[cfg(feature = "cli")]
#[cfg_attr(docsrs, doc(cfg(feature = "cli")))]
pub mod cli;
