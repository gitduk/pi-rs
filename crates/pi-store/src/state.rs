//! What pi keeps between runs that is neither a session, a setting nor a log:
//! one JSON document in the pi root, a section per feature.
//!
//! Sections are read and written whole, so a run that does not know a section
//! carries it through rather than dropping it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

const FILE: &str = ".pi.json";

/// The shared state document for the default or custom pi root.
pub fn path() -> Option<PathBuf> {
    match std::env::var_os("PI_HOME") {
        Some(root) => Some(PathBuf::from(root).join(".pi").join(FILE)),
        None => crate::dir().map(|dir| dir.join(FILE)),
    }
}

/// Every section, as one document.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// How often each skill's instructions reached the model, across sessions.
    #[serde(default)]
    pub skills: BTreeMap<String, SkillLoad>,
    /// Last successful model catalogs grouped by provider name.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::model_cache::deserialize_catalogs"
    )]
    pub models: Option<BTreeMap<String, crate::model_cache::Catalogs>>,
    /// Sections this run has no field for: writing must not drop them.
    #[serde(flatten)]
    pub rest: BTreeMap<String, Value>,
}

/// One skill's history: how often it was loaded, and the size of the last load.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SkillLoad {
    #[serde(default)]
    pub loads: u64,
    #[serde(default)]
    pub bytes: u64,
}

/// The document on disk; missing or unreadable reads as empty.
pub fn read() -> State {
    let Some(path) = path() else {
        return State::default();
    };
    // A document that does not parse reads as empty, so the log is the only
    // thing that can say the counts stopped moving.
    match load(&path) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                target: "pi::session",
                path = %path.display(),
                error = %error,
                "state unreadable"
            );
            State::default()
        }
    }
}

/// Read, change, write back whole. A document that does not parse is refused,
/// not replaced: it may hold sections written by a newer run.
///
/// A separate lock file serializes updates across processes and threads.
pub fn update(change: impl FnOnce(&mut State)) -> std::io::Result<()> {
    let path = path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no pi root to write in")
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Keep the lock on a stable file while the JSON document is replaced.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("json.lock"))?;
    lock.lock()?;
    let mut state = load(&path)?;
    change(&mut state);
    let text = serde_json::to_string_pretty(&state)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string()))?;
    let checked: State = serde_json::from_str(&text)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if checked.models != state.models {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "model catalog does not round-trip through JSON",
        ));
    }
    tool::state::write_private(&path, text.as_bytes())
        .map_err(|error| std::io::Error::new(error.kind(), format!("{}: {error}", path.display())))
}

// No file yet is an empty document, not a failure to read one.
fn load(path: &std::path::Path) -> std::io::Result<State> {
    if let Some(state) = load_existing(path)? {
        return Ok(state);
    }
    // A custom pi root previously kept its state directly under PI_HOME.
    let legacy = std::env::var_os("PI_HOME").map(|root| PathBuf::from(root).join(FILE));
    Ok(match legacy {
        Some(old) => load_existing(&old)?.unwrap_or_default(),
        None => State::default(),
    })
}

fn load_existing(path: &std::path::Path) -> std::io::Result<Option<State>> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{}: {error}", path.display()),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// A skill's instructions reached the model, at this size. Never fails the
/// load: a count that could not be filed is a count, not a lost skill.
pub fn skill_loaded(name: &str, bytes: usize) {
    let filed = update(|state| {
        let load = state.skills.entry(name.to_string()).or_default();
        load.loads += 1;
        load.bytes = bytes as u64;
    });
    if let Err(error) = filed {
        tracing::warn!(
            target: "pi::session",
            skill = name,
            error = %error,
            "skill load not counted"
        );
    }
}
