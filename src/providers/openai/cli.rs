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
///
/// The probe is `--version`, which says the tool is installed and says nothing about whether
/// it is signed in. See [`LocalCli::with_probe`] for why that is the strongest free question
/// here.
pub fn provider(timeout: Duration) -> LocalCli {
    LocalCli::new("codex-cli", PROGRAM, ["exec", "--json"], timeout)
        .reading(envelope())
        .with_model_flag("--model")
        .with_probe(["--version"])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_preset_is_probed() {
        // A preset that shipped without one would answer unknown for every user who never
        // read this far, which is the same as not having the method at all.
        //
        // This lives here rather than in `providers::cli` because it is a fact about this
        // preset, not about the runner. The runner's tests are about the runner.
        assert!(provider(Duration::from_secs(60)).probe.is_some());
    }
}
