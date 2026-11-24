//! Local copies of the last successful provider model lists.

use std::{collections::BTreeMap, fs, io::ErrorKind, path::PathBuf};

use anyhow::{Context, Result};
use ring::hmac;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use crate::config::ModelEntry;
use crate::provider::{Provider, valid_model_id};
use crate::state;

/// Model lists in shared state, with the old directory available for migration.
#[derive(Clone)]
pub struct Cache {
    legacy: PathBuf,
    key: Option<hmac::Key>,
}

/// The lists saved for one provider name, or an old list filed under a hash.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum Catalogs {
    Connections(Vec<Catalog>),
    Legacy(toml::Table),
}

/// A model list and the connection it was read from.
#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct Catalog {
    connection: String,
    models: toml::Table,
}

/// One read of the shared state, reusable across providers in a configuration.
pub struct Snapshot(BTreeMap<String, Catalogs>);

impl Snapshot {
    /// Return valid cached model facts for the matching connection.
    pub fn get(&self, name: &str, connection: &str) -> Option<toml::Table> {
        saved(&self.0, name, connection).map(valid_facts)
    }
}

fn saved<'a>(
    cache: &'a BTreeMap<String, Catalogs>,
    name: &str,
    connection: &str,
) -> Option<&'a toml::Table> {
    if let Some(Catalogs::Connections(catalogs)) = cache.get(name)
        && let Some(catalog) = catalogs.iter().find(|c| c.connection == connection)
    {
        return Some(&catalog.models);
    }
    match cache.get(&format!("v1-{connection}")) {
        Some(Catalogs::Legacy(models)) => Some(models),
        _ => None,
    }
}

fn valid_facts(models: &toml::Table) -> toml::Table {
    models
        .iter()
        .filter_map(|(id, facts)| {
            if !valid_model_id(id) {
                return None;
            }
            let entry: ModelEntry = facts.clone().try_into().ok()?;
            if entry
                .effort
                .is_some_and(|effort| effort != crate::args::EffortArg::Unsupported)
                || [
                    entry.pricing.input_per_mtok,
                    entry.pricing.output_per_mtok,
                    entry.pricing.cache_read_per_mtok,
                    entry.pricing.cache_write_per_mtok,
                ]
                .iter()
                .any(|rate| !rate.is_finite())
            {
                return None;
            }
            Some((id.clone(), facts.clone()))
        })
        .collect()
}

pub(crate) fn deserialize_catalogs<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<BTreeMap<String, Catalogs>>, D::Error> {
    let raw = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(raw.map(|value| {
        let serde_json::Value::Object(entries) = value else {
            tracing::warn!("model catalog cache section is invalid");
            return BTreeMap::new();
        };
        entries
            .into_iter()
            .filter_map(|(name, entry)| {
                let catalog = serde_json::from_value(entry)
                    .inspect_err(|_| {
                        tracing::warn!(provider = name, "model catalog cache entry is invalid")
                    })
                    .ok()?;
                Some((name, catalog))
            })
            .collect()
    }))
}

impl Cache {
    /// The cache under the current pi root.
    pub fn local() -> Option<Self> {
        Some(Self {
            legacy: crate::dir()?.join("model-catalog"),
            key: crate::mask::key().map(|key| hmac::Key::new(hmac::HMAC_SHA256, &key)),
        })
    }

    /// Identify the connection and its effective request credentials.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be serialized or authenticated
    /// cache identity cannot be derived from the local key.
    pub fn fingerprint(
        &self,
        name: &str,
        connection: &Provider,
        key: Option<&str>,
        headers: &[(String, String)],
    ) -> Result<String> {
        if key.is_none() && headers.is_empty() {
            let identity = serde_json::to_vec(&(name, connection))?;
            return Ok(format!("{:x}", Sha256::digest(identity)));
        }
        let signing_key = self
            .key
            .as_ref()
            .context("no local model catalog signing key")?;
        let identity = serde_json::to_vec(&("model-catalog", name, connection, key, headers))?;
        let digest = hmac::sign(signing_key, &identity);
        let hex: String = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(format!("auth-{hex}"))
    }

    fn legacy(&self) -> BTreeMap<String, Catalogs> {
        let files = match fs::read_dir(&self.legacy) {
            Ok(files) => files,
            Err(error) if error.kind() == ErrorKind::NotFound => return BTreeMap::new(),
            Err(error) => {
                tracing::warn!(%error, "model catalog cache migration failed");
                return BTreeMap::new();
            }
        };
        files
            .flatten()
            .filter_map(|file| {
                let path = file.path();
                let key = path.file_name()?.to_str()?.strip_suffix(".toml")?;
                let fingerprint = key.strip_prefix("v1-")?;
                if fingerprint.len() != 64 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return None;
                }
                let read = || -> Result<_> {
                    Ok((
                        key.to_owned(),
                        Catalogs::Legacy(toml::from_str(&fs::read_to_string(&path)?)?),
                    ))
                };
                read()
                    .inspect_err(|error| tracing::warn!(%error, "model catalog cache read failed"))
                    .ok()
            })
            .collect()
    }

    /// Load the saved catalogs once, falling back to the old directory when
    /// the state has no model snapshot yet.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot(state::read().models.unwrap_or_else(|| self.legacy()))
    }

    /// Replace the snapshot. On failure, keep the last saved list for the
    /// matching connection if the caller has no in-memory list.
    ///
    /// # Errors
    ///
    /// Returns an error if the snapshot cannot be serialized or written.
    pub fn write<'a>(
        &self,
        lists: impl IntoIterator<Item = (String, String, Option<&'a toml::Table>)>,
    ) -> Result<()> {
        state::update(|state| {
            let previous = state.models.take().unwrap_or_else(|| self.legacy());
            state.models = Some(
                lists
                    .into_iter()
                    .filter_map(|(name, connection, models)| {
                        let models = models
                            .map(valid_facts)
                            .or_else(|| saved(&previous, &name, &connection).map(valid_facts))?;
                        Some((
                            name,
                            Catalogs::Connections(vec![Catalog { connection, models }]),
                        ))
                    })
                    .collect(),
            );
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bad_saved_catalog_does_not_poison_other_state() {
        let input = r#"{
            "skills": {"example": {"loads": 1, "bytes": 20}},
            "models": {
                "broken": [{"connection":"old","models":{"a":{"pricing":{"input_per_mtok":null}}}}],
                "valid": [{"connection":"current","models":{"b":{"context_window":100000}}}]
            },
            "future": true
        }"#;
        let state: state::State = serde_json::from_str(input).unwrap();
        assert_eq!(state.skills["example"].loads, 1);
        assert_eq!(state.rest["future"], true);
        let catalogs = state.models.unwrap();
        assert!(saved(&catalogs, "broken", "old").is_none());
        assert_eq!(
            saved(&catalogs, "valid", "current").unwrap()["b"]["context_window"].as_integer(),
            Some(100000)
        );
    }
}
