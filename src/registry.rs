//! A table of models, what they can do, and when somebody last checked.

use crate::model::{ModelCapabilities, ModelId, Reach};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One model's entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    /// The model name, as its provider spells it.
    pub id: String,
    /// How many tokens fit in one request.
    pub context_window: u32,
    /// The most tokens it will produce in one reply.
    pub max_output: u32,
    /// Whether it can be given tools.
    #[serde(default)]
    pub tools: bool,
    /// Whether it can be asked for output matching a schema.
    #[serde(default)]
    pub structured_output: bool,
    /// Whether repeated prefixes can be cached.
    #[serde(default)]
    pub prompt_caching: bool,
    /// Whether it can be asked to reason.
    #[serde(default)]
    pub thinking: bool,
    /// Whether the reply can be read as it arrives.
    ///
    /// Defaults to false like the others, so a table written before this column existed
    /// says "no" rather than claiming something nobody checked.
    #[serde(default)]
    pub streaming: bool,
    /// Where the facts on this row came from.
    ///
    /// Required. A capability table with no provenance is a set of claims, and the first
    /// time one of them is wrong there is no way to find out which of them still hold.
    pub source: String,
    /// When a person last checked this row, as `YYYY-MM-DD`.
    pub verified_at: String,
}

/// A table of models for one reach.
///
/// The reach belongs to the table rather than the row, because the same model reached two
/// ways is two sets of capabilities. A vendor CLI usually cannot take a tool schema even
/// when the model behind it can.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    /// Which vendor these models belong to.
    pub provider: String,
    /// How these models are reached.
    pub reach: Reach,
    /// The models, by name.
    #[serde(default, rename = "model", with = "entries")]
    pub models: BTreeMap<String, Entry>,
}

// TOML writes an array of tables and this reads it into a map keyed by id. A duplicate id
// is refused rather than allowed to overwrite, because the row that wins is whichever came
// last in the file and nothing anywhere would say so.
mod entries {
    use super::Entry;
    use serde::de::Error;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::collections::BTreeMap;

    pub fn serialize<S: Serializer>(
        models: &BTreeMap<String, Entry>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        models.values().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, Entry>, D::Error> {
        let rows = Vec::<Entry>::deserialize(deserializer)?;
        let mut models = BTreeMap::new();
        for row in rows {
            if models.contains_key(&row.id) {
                return Err(D::Error::custom(format!(
                    "{} appears twice. One row would silently replace the other, and \
                     nothing would say which",
                    row.id
                )));
            }
            models.insert(row.id.clone(), row);
        }
        Ok(models)
    }
}

impl Registry {
    /// An empty registry for a provider that has no table yet.
    ///
    /// Every lookup returns `None`, which reads as "this provider does not know that
    /// model". That is the honest answer when nobody has written the table down.
    pub fn empty(provider: impl Into<String>, reach: Reach) -> Registry {
        Registry {
            provider: provider.into(),
            reach,
            models: BTreeMap::new(),
        }
    }

    /// Reads a registry from TOML.
    ///
    /// # Errors
    ///
    /// Returns a message when the document cannot be parsed, when a row appears twice, or
    /// when a row has no source or no verification date.
    pub fn parse(text: &str) -> std::result::Result<Registry, String> {
        let registry: Registry = toml::from_str(text).map_err(|e| e.to_string())?;
        for entry in registry.models.values() {
            if entry.source.trim().is_empty() || entry.verified_at.trim().is_empty() {
                return Err(format!(
                    "{}: a row needs a source and a verification date, or nobody can tell \
                     later which of these numbers still hold",
                    entry.id
                ));
            }
        }
        Ok(registry)
    }

    /// What this model can do, as reached this way.
    pub fn capabilities(&self, model: &ModelId) -> Option<ModelCapabilities> {
        let entry = self.models.get(model.as_str())?;
        Some(ModelCapabilities {
            context_window: entry.context_window,
            max_output: entry.max_output,
            tools: entry.tools,
            structured_output: entry.structured_output,
            prompt_caching: entry.prompt_caching,
            thinking: entry.thinking,
            streaming: entry.streaming,
            reach: self.reach,
        })
    }

    /// Every model in the table, in name order.
    pub fn ids(&self) -> Vec<ModelId> {
        self.models.keys().map(|id| ModelId(id.clone())).collect()
    }

    /// Names this table has that the provider no longer serves.
    ///
    /// Reported rather than removed. A row that vanished because a vendor retired a model
    /// is a decision somebody should make, and a table that quietly edits itself is one
    /// nobody can review.
    pub fn stale(&self, served: &[ModelId]) -> Vec<&str> {
        let served: std::collections::BTreeSet<&str> = served.iter().map(|m| m.as_str()).collect();
        self.models
            .keys()
            .map(String::as_str)
            .filter(|id| !served.contains(id))
            .collect()
    }

    /// Names the provider serves that this table has never heard of.
    pub fn unlisted<'a>(&self, served: &'a [ModelId]) -> Vec<&'a ModelId> {
        served
            .iter()
            .filter(|m| !self.models.contains_key(m.as_str()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_ROW: &str = r#"
provider = "test"
reach = "FirstPartyApi"

[[model]]
id = "test-model"
context_window = 200000
max_output = 8192
tools = true
thinking = true
source = "docs"
verified_at = "2026-08-28"
"#;

    #[test]
    fn a_row_reads_back_as_capabilities_for_the_tables_reach() {
        let registry = Registry::parse(ONE_ROW).unwrap_or(Registry::empty("x", Reach::LocalCli));
        let caps = registry.capabilities(&"test-model".into());
        assert_eq!(caps.map(|c| c.reach), Some(Reach::FirstPartyApi));
        assert_eq!(caps.map(|c| c.tools), Some(true));
        assert_eq!(caps.map(|c| c.structured_output), Some(false));
    }

    #[test]
    fn a_model_the_table_does_not_have_is_none() {
        let registry = Registry::parse(ONE_ROW).unwrap_or(Registry::empty("x", Reach::LocalCli));
        assert_eq!(registry.capabilities(&"nothing".into()), None);
    }

    #[test]
    fn a_duplicate_row_is_refused_rather_than_overwritten() {
        let doubled = format!(
            "{ONE_ROW}\n{}",
            ONE_ROW
                .split_once("\n[[model]]")
                .map(|(_, rest)| format!("[[model]]{rest}"))
                .unwrap_or_default()
        );
        let refused = Registry::parse(&doubled);
        assert!(refused.is_err(), "one row silently replaced the other");
    }

    #[test]
    fn a_row_with_no_provenance_is_refused() {
        let anonymous = ONE_ROW.replace("source = \"docs\"", "source = \"\"");
        assert!(Registry::parse(&anonymous).is_err());
    }

    #[test]
    fn a_table_reports_what_the_vendor_stopped_serving() {
        let registry = Registry::parse(ONE_ROW).unwrap_or(Registry::empty("x", Reach::LocalCli));
        assert_eq!(registry.stale(&[]), vec!["test-model"]);
        assert_eq!(registry.stale(&["test-model".into()]), Vec::<&str>::new());
    }

    #[test]
    fn a_table_reports_what_the_vendor_added() {
        let registry = Registry::parse(ONE_ROW).unwrap_or(Registry::empty("x", Reach::LocalCli));
        let served = vec![ModelId::from("test-model"), ModelId::from("brand-new")];
        assert_eq!(
            registry.unlisted(&served),
            vec![&ModelId::from("brand-new")]
        );
    }

    #[test]
    fn an_empty_registry_answers_nothing_rather_than_guessing() {
        let empty = Registry::empty("test", Reach::LocalCli);
        assert_eq!(empty.capabilities(&"anything".into()), None);
    }
}
