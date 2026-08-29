//! The Codex command line tool.
//!
//! Same shape as every other preset: a program, its arguments, and what it prints. The
//! running, the deadline and the failure cases are [`LocalCli`]'s.
//!
//! It sits under `openai` because that is whose tool it is. What it can carry has nothing to
//! do with `providers::openai::api` beside it, and everything to do with being a
//! subprocess — which is what [`crate::Reach`] on its capabilities says, and the module path
//! does not.

use crate::providers::cli::{Envelope, LocalCli, UsageNames};
use std::time::Duration;

/// The program this preset runs.
pub const PROGRAM: &str = "codex";

/// What the tool prints in JSON mode.
///
/// The usage field names are OpenAI's rather than Anthropic's, which is the whole reason
/// [`UsageNames`] is a value and not a constant.
pub fn envelope() -> Envelope {
    Envelope::at("/output").with_usage(
        "/usage",
        UsageNames {
            // This shape reports the whole prompt including the cached part, so what lands
            // in `input_tokens` here is a total rather than the remainder. Named honestly
            // rather than silently: correcting it needs a subtraction the envelope cannot
            // do, and a wrong number that looks right is worse than one that is absent.
            input: "input_tokens".into(),
            cache_read: "cached_input_tokens".into(),
            cache_write: String::new(),
            output: "output_tokens".into(),
        },
    )
}

/// A provider that runs the Codex tool.
///
/// It knows no models until you name them, for the same reason every command line provider
/// does not: the tool cannot be asked.
pub fn provider(timeout: Duration) -> LocalCli {
    LocalCli::new("codex-cli", PROGRAM, ["exec", "--json"], timeout)
        .reading(envelope())
        .with_model_flag("--model")
}
