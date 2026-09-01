//! The config as a tree of paths, so one command can reach all of it.
//!
//! There is no table of settings here: the tree is `Config` serialized, so a
//! field added to the struct appears without anything else being edited.

use anyhow::{Result, bail};

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

fn render(value: &toml::Value) -> String {
    match value {
        toml::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The value at a path, or an error naming the nearest known paths.
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
// a `/settings set` may add a section the file never had. A name that exists
// but is not a table is still refused — the typo that would otherwise be
// swallowed is caught one level deeper, by the config's `deny_unknown_fields`
// when the tree is deserialized, which is what both callers do before
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
    use serde::Deserialize;

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
    fn leaves_walk_every_value() {
        let paths: Vec<_> = leaves(&tree()).into_iter().map(|(p, _)| p).collect();
        for want in [
            "base_url",
            "models.flash.context_window",
            "theme.diff.add",
            "keys.\"edit.insert.newline\"",
        ] {
            assert!(paths.contains(&want.to_string()), "missing {want}");
        }
    }

    #[test]
    fn set_parses_after_the_existing_type() {
        let mut t = tree();
        set(&mut t, "models.flash.context_window", "200_000").unwrap();
        assert_eq!(
            get(&t, "models.flash.context_window").unwrap(),
            &toml::Value::Integer(200_000)
        );
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

    #[test]
    fn a_new_section_is_created_for_a_path_that_did_not_exist() {
        let mut t = tree();
        set(&mut t, "models.deepseek.context_window", "200_000").unwrap();
        let v = get(&t, "models.deepseek.context_window").unwrap();
        assert_eq!(v, &toml::Value::Integer(200_000));
    }

    #[test]
    fn a_typo_near_a_real_path_is_refused_by_the_config() {
        let mut t = tree();
        // The write lands (a new section is a legitimate target); the typo is
        // caught where `/settings` actually validates, the config's
        // `deny_unknown_fields`, so a silently inert setting never applies.
        set(&mut t, "theme.dif.add", "#fff").unwrap();
        let err = crate::config::Config::deserialize(t)
            .unwrap_err()
            .to_string();
        assert!(err.contains("dif"), "{err}");
    }
}
