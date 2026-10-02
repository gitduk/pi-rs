//! The config files as one tree: the user's, with the project's `.pi.toml`
//! laid over it key by key.
//!
//! There is no session layer: `/settings` opens the project's file in an
//! editor, and `/reload` re-reads both.

use std::path::{Path, PathBuf};

use anyhow::Result;

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

/// The config files as this run read them.
pub struct Settings {
    // The user's tree with the project's over it, as last read from disk.
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

    /// Whether either file sets the top-level `key`.
    pub fn sets(&self, key: &str) -> bool {
        self.file.get(key).is_some()
    }

    /// The project file, when it is the one setting the top-level `key`.
    pub fn project_sets(&self, key: &str) -> Option<&Path> {
        let (file, tree) = self.project.as_ref()?;
        tree.get(key).map(|_| file.as_path())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tables merge, don't replace, and each file's own keys are tracked separately.
    #[test]
    fn the_project_file_lays_over_the_user_file_key_by_key() {
        let user = toml::from_str(
            "base_url = \"http://user\"\nmodel = \"a\"\n[vim]\nenabled = false\nescape = \"jk\"\n",
        )
        .unwrap();
        let project =
            toml::from_str("base_url = \"http://project\"\n[vim]\nenabled = true\n").unwrap();
        let s = Settings::new(user, Some((PathBuf::from("/repo/.pi.toml"), project)));
        assert_eq!(s.file["base_url"].as_str(), Some("http://project"));
        assert_eq!(s.file["model"].as_str(), Some("a"));
        assert_eq!(s.file["vim"]["enabled"].as_bool(), Some(true));
        assert_eq!(s.file["vim"]["escape"].as_str(), Some("jk"));
        assert_eq!(
            s.project_sets("base_url"),
            Some(Path::new("/repo/.pi.toml"))
        );
        assert_eq!(s.project_sets("model"), None);
        assert!(s.sets("model") && !s.sets("effort"));
    }
}
