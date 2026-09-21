//! The config as a tree of paths, so one command can reach all of it.
//!
//! There is no table of settings here: the tree is `Config` serialized, so a
//! field added to the struct appears without anything else being edited.
//!
//! `Settings` is the pair a run carries: the tree as the file last said it, and
//! the values this session claimed on top. What the config is computed from is
//! the two overlaid; what the panel edits is the claim, and `/reload` replaces
//! the file's half.

use std::borrow::Cow;
use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde::Deserialize as _;

/// Every leaf, as `path` and the value rendered the way a file would write it.
pub fn leaves(tree: &toml::Value) -> Vec<(String, String)> {
    fn walk(value: &toml::Value, prefix: &str, out: &mut Vec<(String, String)>) {
        match value {
            toml::Value::Table(map) => {
                for (k, v) in map {
                    let next = if prefix.is_empty() {
                        segment(k)
                    } else {
                        format!("{prefix}.{}", segment(k))
                    };
                    walk(v, &next, out);
                }
            }
            leaf => out.push((prefix.to_string(), render(leaf))),
        }
    }
    let mut out = Vec::new();
    walk(tree, "", &mut out);
    out
}

/// One leaf as the panel and the read-only list show it: the value in force
/// for the session, and whether the file still holds something else — which
/// is what the panel's write and revert act on.
pub struct SettingRow {
    pub path: String,
    pub value: String,
    pub changed: bool,
}

/// The config file as this run read it, and the values this session claimed on
/// top of it.
pub struct Settings {
    // The tree as last read from disk. `/settings` edits a copy of it;
    // `/reload` replaces it.
    file: toml::Value,
    // What this session has claimed, by path. Replayed over every reload, so a
    // claimed value keeps winning over the file.
    claimed: BTreeMap<String, toml::Value>,
}

impl Settings {
    pub fn new(file: toml::Value) -> Self {
        Self {
            file,
            claimed: BTreeMap::new(),
        }
    }

    /// Re-read the file, keeping what this session has claimed.
    pub fn reread(&mut self, at: Option<&str>) -> Result<()> {
        self.file = crate::store::config::load_tree(at)?;
        Ok(())
    }

    /// The file tree with the claims on top — what the config is computed from.
    pub fn effective(&self) -> Result<toml::Value> {
        let mut tree = self.file.clone();
        for (path, value) in &self.claimed {
            put(&mut tree, path, value.clone())?;
        }
        Ok(tree)
    }

    pub fn claimed(&self) -> &BTreeMap<String, toml::Value> {
        &self.claimed
    }

    /// What the file alone says at `path`, for the check that names a claim
    /// still shadowing a line the file has moved on from.
    pub fn file_value(&self, path: &str) -> Option<&toml::Value> {
        get(&self.file, path).ok()
    }

    /// What this session has claimed at `path`, if anything.
    pub fn claimed_value(&self, path: &str) -> Option<toml::Value> {
        self.claimed.get(path).cloned()
    }

    /// Take a value into the session, or refuse it whole: the write is tried on
    /// a scratch tree first, so a value the config would not accept reaches
    /// neither the config in force nor the claim. Answers with the value that
    /// was there and the one that now is.
    pub fn claim(&mut self, path: &str, raw: &str) -> Result<(Option<toml::Value>, toml::Value)> {
        let raw = typed(path, raw);
        let mut scratch = self.effective()?;
        let old = get(&scratch, path).ok().cloned();
        set(&mut scratch, path, &raw)?;
        let new = get(&scratch, path).expect("the path was just set").clone();
        crate::store::config::Config::deserialize(scratch).map_err(|e| anyhow::anyhow!(e))?;
        self.claimed.insert(path.to_string(), new.clone());
        Ok((old, new))
    }

    /// Plant a claim no line of the file can address, which is what the panel
    /// has to keep answering for. No path in the program makes one — `claim`
    /// refuses what the config will not take — so this is the tests' way in.
    #[cfg(test)]
    pub(crate) fn claim_unchecked(&mut self, path: &str, value: toml::Value) {
        self.claimed.insert(path.to_string(), value);
    }

    /// Drop this session's claim on `path`. False when there was none.
    pub fn drop_claim(&mut self, path: &str) -> bool {
        self.claimed.remove(path).is_some()
    }

    /// The file's rows with the session's claims on top — what the panel shows
    /// and the read-only list prints. Path by path rather than one overlaid
    /// tree, so a claim the file can no longer address (an ancestor the file
    /// has turned into a non-table) still answers, with the file's own value
    /// beside it for the mark.
    pub fn rows(&self) -> Vec<SettingRow> {
        let mut rows: BTreeMap<String, String> = leaves(&self.file).into_iter().collect();
        for (path, claimed) in &self.claimed {
            rows.insert(path.clone(), render(claimed));
        }
        rows.into_iter()
            .map(|(path, value)| {
                let claimed = self.claimed.get(&path);
                let file = get(&self.file, &path).ok();
                SettingRow {
                    path,
                    value,
                    changed: claimed.is_some() && claimed != file,
                }
            })
            .collect()
    }
}

/// What the panel and the read-only list show in place of a value the journal
/// redacts: whether there is one, never which.
pub(crate) fn mask_secret(path: &str, value: &str) -> String {
    if crate::store::journal::secret(crate::store::journal::leaf(path)) {
        match value {
            "" => "<unset>".to_string(),
            _ => "<set>".to_string(),
        }
    } else {
        value.to_string()
    }
}

/// The value a path takes, before it is written: `base_url` names a host that
/// may be spelled with the environment's own variables in it.
fn typed<'a>(path: &str, raw: &'a str) -> Cow<'a, str> {
    match path {
        "base_url" => Cow::Owned(crate::store::config::expand_base_url(raw)),
        _ => Cow::Borrowed(raw),
    }
}

#[cfg(test)]
pub(crate) fn row(path: &str, value: &str, changed: bool) -> SettingRow {
    SettingRow {
        path: path.into(),
        value: value.into(),
        changed,
    }
}

pub(crate) fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The value at `path`, or an error when nothing sits there.
pub fn get<'a>(tree: &'a toml::Value, path: &str) -> Result<&'a toml::Value> {
    let mut at = tree;
    for part in segments(path)? {
        let toml::Value::Table(map) = at else {
            bail!("`{path}` is not a table path");
        };
        at = map.get(&part).ok_or_else(|| unknown(path))?;
    }
    Ok(at)
}

/// Write `raw` at `path`, typed after the value already there.
pub fn set(tree: &mut toml::Value, path: &str, raw: &str) -> Result<()> {
    let segments = segments(path)?;
    let parent = table_at(tree, &segments[..segments.len() - 1], path)?;
    let key = &segments[segments.len() - 1];
    let value = match parent.get(key) {
        Some(cur) => parse_after(cur, raw)?,
        None => raw
            .parse::<toml::Value>()
            .unwrap_or(toml::Value::String(raw.to_string())),
    };
    parent.insert(key.clone(), value);
    Ok(())
}

/// Place an already-parsed value at `path`, creating intermediate tables.
/// Used by `/settings`'s replay log, where the value was validated when it
/// was claimed and must land in the tree exactly as typed.
pub fn put(tree: &mut toml::Value, path: &str, value: toml::Value) -> Result<()> {
    let segments = segments(path)?;
    let parent = table_at(tree, &segments[..segments.len() - 1], path)?;
    parent.insert(segments[segments.len() - 1].clone(), value);
    Ok(())
}

// A segment with a dot in it is quoted: keys."edit.insert.newline". The rest
// split on the bare dot.
pub(crate) fn segments(path: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    let mut at = 0;
    let bytes = path.as_bytes();
    while at < path.len() {
        if bytes[at] == b'"' {
            let rest = &path[at + 1..];
            let end = rest
                .find('"')
                .ok_or_else(|| anyhow::anyhow!("unclosed quote in `{path}`"))?;
            out.push(rest[..end].to_string());
            at += end + 2;
            if at < path.len() && bytes[at] == b'.' {
                at += 1;
            }
        } else {
            let end = path[at..].find('.').map(|i| at + i).unwrap_or(path.len());
            out.push(path[at..end].to_string());
            at = end + 1;
        }
    }
    if out.is_empty() {
        bail!("empty path");
    }
    Ok(out)
}

// The table the path's parent names, creating missing tables along the way:
// a write may add a section the file never had. A name that exists
// but is not a table is still refused — the typo that would otherwise be
// swallowed is caught one level deeper, by the config's `deny_unknown_fields`
// when the tree is deserialized, which is what the caller does before
// anything is applied.
fn table_at<'a>(
    tree: &'a mut toml::Value,
    parts: &[String],
    path: &str,
) -> Result<&'a mut toml::Table> {
    let mut at = tree;
    for part in parts {
        let toml::Value::Table(map) = at else {
            bail!("`{path}` is not a table path");
        };
        at = map
            .entry(part.clone())
            .or_insert_with(|| toml::Value::Table(Default::default()));
    }
    let toml::Value::Table(map) = at else {
        bail!("`{path}` is not a table path");
    };
    Ok(map)
}

fn unknown(path: &str) -> anyhow::Error {
    anyhow::anyhow!("no setting `{path}`")
}

fn segment(key: &str) -> String {
    if key.contains('.') {
        format!("\"{key}\"")
    } else {
        key.to_string()
    }
}

// The value already there decides the type. `Integer` → i64, `Boolean` → bool,
// `Float` → f64, `String` → as-is, `Array` → parsed as a TOML array.
fn parse_after(cur: &toml::Value, raw: &str) -> Result<toml::Value> {
    Ok(match cur {
        toml::Value::Integer(_) => toml::Value::Integer(raw.replace('_', "").parse()?),
        toml::Value::Boolean(_) => toml::Value::Boolean(raw.parse()?),
        toml::Value::Float(_) => toml::Value::Float(raw.replace('_', "").parse()?),
        toml::Value::String(_) => toml::Value::String(raw.to_string()),
        toml::Value::Array(_) => raw.parse::<toml::Value>()?,
        other => bail!("cannot set `{other:?}` from a string"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r##"
base_url = "http://localhost:7896/v1"
format = "openai"
api_key = "x"
model = "flash"
effort = "medium"

[models.flash]
context_window = 1_000_000

[theme.diff]
add = "#58a6ff"

[keys]
"edit.insert.newline" = ["ctrl+j"]
"##;

    fn tree() -> toml::Value {
        toml::from_str(SAMPLE).unwrap()
    }

    #[test]
    fn a_wrong_type_changes_nothing() {
        let mut t = tree();
        let before = t.clone();
        assert!(set(&mut t, "models.flash.context_window", "six").is_err());
        assert_eq!(t, before);
    }

    #[test]
    fn a_quoted_segment_reaches_a_key_with_a_dot() {
        let mut t = tree();
        set(&mut t, "keys.\"edit.insert.newline\"", "[\"ctrl+k\"]").unwrap();
        let v = get(&t, "keys.\"edit.insert.newline\"").unwrap();
        assert!(v.is_array());
    }
}
