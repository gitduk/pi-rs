//! The config as a tree of paths, so one command can reach all of it.
//!
//! There is no table of settings here: the tree is `Config` serialized, so a
//! field added to the struct appears without anything else being edited.
//!
//! `Settings` is what a run carries: the tree as the files last said it — the
//! user's, with the project's `.pi.toml` laid over it. There is no session
//! layer: the panel writes the project's file, and `/reload` re-reads both.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

// `over`'s keys replace `base`'s, table by table; anything else replaces whole.
fn overlay(base: &mut toml::Value, over: &toml::Value) {
    match (base, over) {
        (toml::Value::Table(base), toml::Value::Table(over)) => {
            for (k, v) in over {
                match base.get_mut(k) {
                    Some(slot) => overlay(slot, v),
                    None => {
                        base.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

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

/// One leaf as the panel shows it: where it is, and what the files say.
pub struct SettingRow {
    pub path: String,
    pub value: String,
}

/// The config files as this run read them.
pub struct Settings {
    // The user's tree with the project's over it, as last read from disk.
    // `/settings` edits a copy of it; `/reload` replaces it.
    file: toml::Value,
    // The project's file and its own tree, to say which keys it set.
    project: Option<(PathBuf, toml::Value)>,
}

impl Settings {
    pub fn new(user: toml::Value, project: Option<(PathBuf, toml::Value)>) -> Self {
        let mut file = user;
        if let Some((_, tree)) = &project {
            overlay(&mut file, tree);
        }
        Self { file, project }
    }

    /// The user's file (`at`, else the default) and the project file nearest
    /// `root`.
    pub fn load(at: Option<&str>, root: &Path) -> Result<Self> {
        Ok(Self::new(
            crate::store::config::load_tree(at)?,
            crate::store::config::load_project(root)?,
        ))
    }

    /// Re-read both files.
    pub fn reread(&mut self, at: Option<&str>, root: &Path) -> Result<()> {
        *self = Self::load(at, root)?;
        Ok(())
    }

    /// The config in force: the files, then the environment.
    pub fn config(&self) -> Result<crate::store::config::Config> {
        crate::store::config::Config::in_force(self.file.clone())
    }

    /// The project file read, if one was found.
    pub fn project(&self) -> Option<&Path> {
        self.project.as_ref().map(|(file, _)| file.as_path())
    }

    /// The project file, when it is the one holding a value at `path`.
    pub fn project_sets(&self, path: &str) -> Option<&Path> {
        let (file, tree) = self.project.as_ref()?;
        get(tree, path).ok().map(|_| file.as_path())
    }

    /// `raw` at `path`, tried on a copy of the files first: a value the config
    /// would not accept reaches no file. Answers with the value that was there
    /// and the one to write.
    pub fn check(&self, path: &str, raw: &str) -> Result<(Option<toml::Value>, toml::Value)> {
        let raw = typed(path, raw);
        let mut scratch = self.file.clone();
        let old = get(&scratch, path).ok().cloned();
        set(&mut scratch, path, &raw)?;
        let new = get(&scratch, path).expect("the path was just set").clone();
        crate::store::config::Config::from_tree(scratch)?;
        Ok((old, new))
    }

    /// Every leaf the files hold — what the panel shows.
    pub fn rows(&self) -> Vec<SettingRow> {
        leaves(&self.file)
            .into_iter()
            .map(|(path, value)| SettingRow { path, value })
            .collect()
    }
}

/// What the panel shows in place of a value the journal redacts: whether there
/// is one, never which.
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
pub(crate) fn row(path: &str, value: &str) -> SettingRow {
    SettingRow {
        path: path.into(),
        value: value.into(),
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

    // The project's file wins key by key, tables merged rather than replaced,
    // and says which keys are its own.
    #[test]
    fn the_project_file_lays_over_the_user_file_key_by_key() {
        let user = toml::from_str(
            "base_url = \"http://user\"\nmodel = \"a\"\n[vim]\nenabled = false\nescape = \"jk\"\n",
        )
        .unwrap();
        let project =
            toml::from_str("base_url = \"http://project\"\n[vim]\nenabled = true\n").unwrap();
        let s = Settings::new(user, Some((PathBuf::from("/repo/.pi.toml"), project)));
        let tree = s.file.clone();
        assert_eq!(
            get(&tree, "base_url").unwrap().as_str(),
            Some("http://project")
        );
        assert_eq!(get(&tree, "model").unwrap().as_str(), Some("a"));
        assert_eq!(get(&tree, "vim.enabled").unwrap().as_bool(), Some(true));
        assert_eq!(get(&tree, "vim.escape").unwrap().as_str(), Some("jk"));
        assert_eq!(
            s.project_sets("base_url"),
            Some(Path::new("/repo/.pi.toml"))
        );
        assert_eq!(s.project_sets("model"), None);
    }

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
