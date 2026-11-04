//! The config files as one tree: the user's, with the project's `.pi/settings.toml`
//! laid over it key by key.
//!
//! There is no session layer: the files are edited in place, and a reload
//! re-reads both.

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

// A credential or header inherited from another layer belongs to that
// layer's host. A replacement target needs its own.
fn clear_replaced_credentials(base: &mut toml::Value, over: &toml::Value) {
    let Some(providers) = base.get_mut("provider").and_then(toml::Value::as_table_mut) else {
        return;
    };
    let patches = over.get("provider").and_then(toml::Value::as_table);
    for (name, patch) in patches.into_iter().flatten() {
        let Some(provider) = providers.get_mut(name).and_then(toml::Value::as_table_mut) else {
            continue;
        };
        let replaced = ["base_url", "models_url"].into_iter().any(|field| {
            patch
                .get(field)
                .is_some_and(|value| provider.get(field) != Some(value))
        });
        if replaced {
            // Overlay re-adds what the patch states; a table would otherwise
            // keep entries written for the old host.
            provider.remove("api_key");
            provider.remove("headers");
            provider.remove("session_header");
        }
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
            clear_replaced_credentials(&mut file, tree);
            overlay(&mut file, tree);
        }
        Self { file, project }
    }

    /// The user's file (`at`, else the default) and the project file nearest
    /// `root`.
    pub fn load(at: Option<&str>, root: &Path) -> Result<Self> {
        Ok(Self::new(
            crate::config::load_tree(at)?,
            crate::config::load_project(root)?,
        ))
    }

    /// The config in force, with provider identities resolved.
    pub fn config(&self) -> Result<crate::config::Config> {
        crate::config::Config::in_force(self.file.clone())
    }

    /// Discovered facts underneath the files: explicit fields always win.
    pub fn config_with_models(&self, models: &toml::Table) -> Result<crate::config::Config> {
        let config = self.config()?;
        let mut file = self.file.clone();
        if let Some(written) = file.get_mut("models").and_then(toml::Value::as_table_mut) {
            for (name, entry) in std::mem::take(written) {
                written.insert(config.find(&name)?.model, entry);
            }
        }
        let models = models.clone();
        let discovered = models.keys().cloned().collect();
        let mut tree = toml::Value::Table(toml::Table::from_iter([(
            "models".to_string(),
            toml::Value::Table(models),
        )]));
        overlay(&mut tree, &file);
        let mut config = crate::config::Config::in_force(tree)?;
        config.discovered = discovered;
        Ok(config)
    }

    /// The project file read, if one was found.
    pub fn project(&self) -> Option<&Path> {
        self.project.as_ref().map(|(file, _)| file.as_path())
    }

    /// Whether either file sets the top-level `key`.
    pub fn sets(&self, key: &str) -> bool {
        self.file.get(key).is_some()
    }

    /// The project file and its own tree, if one was found.
    pub fn project_tree(&self) -> Option<(&Path, &toml::Value)> {
        self.project
            .as_ref()
            .map(|(file, tree)| (file.as_path(), tree))
    }

    /// The project file and the keys it sets that reach past its checkout,
    /// when it sets any.
    pub fn project_reaching(&self) -> Option<(&Path, Vec<(String, String)>)> {
        let (file, tree) = self.project.as_ref()?;
        let keys = crate::config::reaching(tree);
        (!keys.is_empty()).then_some((file.as_path(), keys))
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

    fn with_provider_patch(patch: &str) -> Settings {
        Settings::new(
            toml::from_str(
                r#"
[provider.local]
base_url = "https://original.example/v1"
format = "chat"
api_key = "original-key"
"#,
            )
            .unwrap(),
            Some((
                PathBuf::from("/repo/.pi/settings.toml"),
                toml::from_str(patch).unwrap(),
            )),
        )
    }

    #[test]
    fn provider_address_overrides_do_not_inherit_credentials() {
        for patch in [
            "[provider.local]\nbase_url = \"https://other.example/v1\"\n",
            "[provider.local]\nmodels_url = \"https://other.example/models\"\n",
        ] {
            let config = with_provider_patch(patch).config().unwrap();
            assert_eq!(config.key_for("local/m"), None, "patch: {patch}");
        }
        let config = with_provider_patch(
            "[provider.local]\nbase_url = \"https://other.example/v1\"\napi_key = \"replacement-key\"\n",
        ).config().unwrap();
        assert_eq!(
            config.key_for("local/m").as_deref(),
            Some("replacement-key")
        );
    }

    #[test]
    fn provider_address_overrides_do_not_inherit_headers() {
        let user: toml::Value = toml::from_str(
            r#"
[provider.local]
base_url = "https://original.example/v1"
format = "chat"
session_header = "x-original-session"
[provider.local.headers]
authorization = "Bearer original"
"#,
        )
        .unwrap();
        let project = |body: &str| {
            Some((
                PathBuf::from("/repo/.pi/settings.toml"),
                toml::from_str(body).unwrap(),
            ))
        };
        let moved = Settings::new(
            user.clone(),
            project("[provider.local]\nbase_url = \"https://other.example/v1\"\n"),
        )
        .config()
        .unwrap();
        assert!(
            moved.provider["local"].headers.is_empty(),
            "a header for one host must not travel to another"
        );
        assert!(
            moved.provider["local"].session_header.is_none(),
            "the session header names a fact about that host"
        );

        let kept = Settings::new(
            user,
            project(
                "[provider.local]\nbase_url = \"https://other.example/v1\"\n\
                 [provider.local.headers]\nx-session = \"s2\"\n",
            ),
        )
        .config()
        .unwrap();
        assert!(
            !kept.provider["local"].headers.contains_key("authorization"),
            "a header written for the original host is not inherited"
        );
        assert_eq!(
            kept.provider["local"]
                .headers
                .get("x-session")
                .map(String::as_str),
            Some("s2")
        );
    }

    #[test]
    fn provider_mapping_overrides_keep_the_original_connection_credential() {
        let config = with_provider_patch(
            "[provider.local]\nmodels_path = \"models\"\ncontext_window = \"limit.context\"\n",
        )
        .config()
        .unwrap();
        assert_eq!(config.key_for("local/m").as_deref(), Some("original-key"));
        assert_eq!(config.provider["local"].models_path, "models");
        assert_eq!(config.provider["local"].context_window, "limit.context");
    }

    #[test]
    fn provider_files_are_merged_before_required_fields_are_checked() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let user = dir.path().join("user.toml");
        std::fs::create_dir(dir.path().join(".pi")).unwrap();
        std::fs::write(&user, "[provider.local]\nbase_url = \":3425/v1\"\nformat = \"chat\"\napi_key = \"user-key\"\n").unwrap();
        std::fs::write(
            dir.path().join(".pi/settings.toml"),
            "[provider.local]\nmodels_path = \"models\"\n",
        )
        .unwrap();
        let settings = Settings::load(Some(user.to_str().unwrap()), dir.path()).unwrap();
        let config = settings.config().unwrap();
        assert_eq!(
            config.provider["local"].base_url,
            "http://127.0.0.1:3425/v1"
        );
        assert_eq!(config.provider["local"].models_path, "models");
        assert_eq!(config.key_for("local/m").as_deref(), Some("user-key"));

        std::fs::write(&user, "").unwrap();
        let incomplete = Settings::load(Some(user.to_str().unwrap()), dir.path()).unwrap();
        assert!(
            incomplete.config().is_err(),
            "the merged connection must still be complete"
        );
    }

    // Tables merge, don't replace, and each file's own keys are tracked separately.
    #[test]
    fn the_project_file_lays_over_the_user_file_key_by_key() {
        let user = toml::from_str(
            "tier = \"read\"\nmodel = \"a\"\n[vim]\nenabled = false\nescape = \"jk\"\n",
        )
        .unwrap();
        let project = toml::from_str("tier = \"exec\"\n[vim]\nenabled = true\n").unwrap();
        let s = Settings::new(
            user,
            Some((PathBuf::from("/repo/.pi/settings.toml"), project)),
        );
        assert_eq!(s.file["tier"].as_str(), Some("exec"));
        assert_eq!(s.file["model"].as_str(), Some("a"));
        assert_eq!(s.file["vim"]["enabled"].as_bool(), Some(true));
        assert_eq!(s.file["vim"]["escape"].as_str(), Some("jk"));
        assert_eq!(
            s.project_sets("tier"),
            Some(Path::new("/repo/.pi/settings.toml"))
        );
        assert_eq!(s.project_sets("model"), None);
        assert!(s.sets("model") && !s.sets("effort"));
    }

    #[test]
    fn discovered_capabilities_reach_the_model_spec() {
        let s = Settings::new(
            toml::from_str("[provider.local]\nbase_url = \":3425/v1\"\nformat = \"chat\"\n")
                .unwrap(),
            None,
        );
        let models = toml::from_str(
            r#"
["local/a"]
vision = true
accepts_temperature = false
effort = "unsupported"
["local/a".pricing]
input_per_mtok = 0.15
"#,
        )
        .unwrap();
        let spec = s
            .config_with_models(&models)
            .unwrap()
            .find("local/a")
            .unwrap();
        assert!(spec.vision);
        assert!(!spec.accepts_temperature);
        assert!(!spec.accepts_effort);
        assert_eq!(spec.pricing.input_per_mtok, 0.15);
    }

    #[test]
    fn manual_fields_override_discovery_without_erasing_other_facts() {
        let s = Settings::new(
            toml::from_str(
                r#"
[provider.local]
base_url = ":3425/v1"
format = "chat"
[models."local/a"]
context_window = 50000
vision = true
"#,
            )
            .unwrap(),
            None,
        );
        let models = toml::from_str(
            r#"
["local/a"]
context_window = 200000
max_output_tokens = 20000
["local/b"]
context_window = 300000
"#,
        )
        .unwrap();
        let c = s.config_with_models(&models).unwrap();
        let a = c.find("local/a").unwrap();
        assert_eq!(a.context_window, 50000);
        assert_eq!(a.max_output_tokens, 20000);
        assert!(a.vision);
        assert_eq!(c.find("local/b").unwrap().context_window, 300000);
        assert_eq!(c.names(), ["local/a", "local/b"]);
        assert_eq!(
            s.config().unwrap().names(),
            ["local/a"],
            "discovery never changes the file tree"
        );
    }

    #[test]
    fn a_bare_override_for_a_sole_provider_merges_into_its_discovered_entry() {
        let s = Settings::new(
            toml::from_str(
                r#"
[provider.local]
base_url = ":3425/v1"
format = "chat"
[models.a]
max_output_tokens = 1234
"#,
            )
            .unwrap(),
            None,
        );
        let models =
            toml::from_str("[\"local/a\"]\ncontext_window = 50000\nmax_output_tokens = 20000\n")
                .unwrap();
        let config = s.config_with_models(&models).unwrap();
        assert_eq!(config.names(), ["local/a"]);
        let spec = config.find("a").unwrap();
        assert_eq!(spec.model, "local/a");
        assert_eq!(spec.context_window, 50000);
        assert_eq!(spec.max_output_tokens, 1234);
    }
}
