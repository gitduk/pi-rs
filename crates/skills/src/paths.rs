use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// Validated glob patterns relative to a run's workspace.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(try_from = "Vec<String>")]
pub struct Paths {
    patterns: Vec<String>,
    matcher: GlobSet,
}

impl PartialEq for Paths {
    fn eq(&self, other: &Self) -> bool {
        self.patterns == other.patterns
    }
}

impl Eq for Paths {}

impl TryFrom<Vec<String>> for Paths {
    type Error = String;

    fn try_from(patterns: Vec<String>) -> Result<Self, Self::Error> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &patterns {
            if pattern.is_empty() || Path::new(pattern).is_absolute() {
                return Err(format!(
                    "invalid skill path pattern `{pattern}`: use a nonempty workspace-relative glob"
                ));
            }
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("invalid skill path pattern `{pattern}`: {e}"))?;
            builder.add(glob);
        }
        let matcher = builder.build().map_err(|e| e.to_string())?;
        Ok(Self { patterns, matcher })
    }
}

impl Paths {
    /// Whether no patterns are configured.
    pub fn is_empty(&self) -> bool {
        self.matcher.is_empty()
    }

    /// Whether any pattern matches this workspace-relative path.
    pub fn matches(&self, path: &Path) -> bool {
        self.matcher.is_match(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_validated_and_match_workspace_relative_files() {
        let paths: Paths = serde_yaml_ng::from_str("['**/*.rs', 'Cargo.toml']").unwrap();
        assert!(paths.matches(Path::new("lib.rs")));
        assert!(paths.matches(Path::new("crates/agent/src/lib.rs")));
        assert!(paths.matches(Path::new("Cargo.toml")));
        assert!(!paths.matches(Path::new("crates/agent/Cargo.toml")));
        assert!(!paths.matches(Path::new("src/main.py")));
        assert!(!Paths::default().matches(Path::new("lib.rs")));
        for invalid in ["['[']", "['']", "['/tmp/*.rs']", "'**/*.rs'"] {
            assert!(
                serde_yaml_ng::from_str::<Paths>(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }
}
