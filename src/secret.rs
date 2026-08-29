//! A string that does not leak through the obvious channels.

use std::fmt;

/// An API key, or anything else that must not reach a log.
///
/// `Debug` and `Display` print the label and never the value. `Serialize` is deliberately
/// not implemented, so a secret cannot be written into JSON by accident. The buffer is
/// overwritten when the value is dropped.
///
/// None of that makes a secret safe to hold forever. It makes the common accidents into
/// compile errors and unreadable output instead of a key in a log file.
///
/// ```
/// use llmr::Secret;
///
/// let key = Secret::new("api-key", "sk-not-a-real-key");
/// assert_eq!(format!("{key:?}"), "Secret(api-key)");
/// assert_eq!(key.expose_str(), Ok("sk-not-a-real-key"));
/// ```
pub struct Secret {
    bytes: Vec<u8>,
    label: &'static str,
}

impl Secret {
    /// Wraps a value. The label is for diagnostics and is never the value itself.
    pub fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            bytes: value.into().into_bytes(),
            label,
        }
    }

    /// Reads a secret from an environment variable.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::Auth`] when the variable is unset or empty. Empty counts as
    /// unset, because an exported but blank variable is the same problem and reports as a
    /// confusing authentication failure otherwise.
    pub fn from_env(label: &'static str, variable: &str) -> crate::Result<Self> {
        match std::env::var(variable) {
            Ok(value) if !value.trim().is_empty() => Ok(Self::new(label, value)),
            _ => Err(crate::Error::Auth(format!("{variable} is not set"))),
        }
    }

    /// The only way to read the value. Named so it stands out in review and in a search.
    pub fn expose(&self) -> &[u8] {
        &self.bytes
    }

    /// The value as text.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are not valid UTF-8.
    pub fn expose_str(&self) -> std::result::Result<&str, std::str::Utf8Error> {
        std::str::from_utf8(&self.bytes)
    }

    /// What this secret is for.
    pub fn label(&self) -> &'static str {
        self.label
    }

    /// Whether the value is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({})", self.label)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{} redacted]", self.label)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Overwritten rather than just freed. It does not defeat a memory dump taken while
        // the process is alive, and it does shorten the window.
        self.bytes.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_never_carry_the_value() {
        let key = Secret::new("api-key", "sk-super-secret");
        assert!(!format!("{key:?}").contains("secret"));
        assert!(!format!("{key}").contains("secret"));
    }

    #[test]
    fn an_unset_variable_reads_as_an_authentication_problem() {
        let missing = Secret::from_env("api-key", "LLMR_TEST_DEFINITELY_UNSET");
        assert!(matches!(missing, Err(crate::Error::Auth(_))));
    }
}
