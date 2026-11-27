//! Configuration and services owned by one workspace.

use super::mcp;
use crate::input::commands::Command;
use pi_store::{config::Config, settings::Settings};
use std::sync::Arc;

/// Runtime settings and service handles belonging to one workspace.
pub struct Environment {
    pub(crate) config: Arc<Config>,
    pub(crate) settings: Settings,
    pub(crate) commands: Arc<Vec<Command>>,
    pub(crate) prompts_seen: u64,
    pub(crate) refused: Option<Vec<tool::stamp::Stamp>>,
    pub(crate) mcp: mcp::Live,
}

impl Environment {
    /// Adopt the configuration and service handle resolved for this workspace.
    pub fn new(
        config: Arc<Config>,
        settings: Settings,
        commands: Arc<Vec<Command>>,
        mcp: mcp::Live,
    ) -> Self {
        Self {
            config,
            settings,
            commands,
            mcp,
            prompts_seen: 0,
            refused: None,
        }
    }

    pub(crate) fn install(
        &mut self,
        config: Config,
        settings: Option<Settings>,
        root: &std::path::Path,
    ) {
        if let Some(settings) = settings {
            self.settings = settings;
        }
        self.mcp.sync(&config, root);
        self.config = Arc::new(config);
        self.refused = None;
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn fixture(commands: Arc<Vec<Command>>) -> Self {
        Self::new(
            Arc::default(),
            Settings::new(toml::Table::new().into(), None),
            commands,
            mcp::Live::default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_hidden_workspace_reloads_its_own_settings_without_switching_selection() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for root in [first.path(), second.path()] {
            std::fs::create_dir_all(root.join(".pi")).unwrap();
        }
        std::fs::write(first.path().join(".pi/settings.toml"), "retries = 2\n").unwrap();
        let second_file = second.path().join(".pi/settings.toml");
        std::fs::write(&second_file, "retries = 7\n").unwrap();
        let mut core = crate::core::tests::a_repl(first.path(), Arc::default(), "m");
        core.try_reload().unwrap();
        core.open_lane(tool::Workspace::new(second.path()).unwrap(), 0)
            .unwrap();
        assert_eq!(core.config().retries, Some(7));
        core.lanes.show(0);
        assert_eq!(core.config().retries, Some(2));
        let selected = core.config().clone();
        std::fs::write(&second_file, "retries = 11\n").unwrap();
        assert!(core.refresh_config_at(1).is_some());
        assert_eq!(core.lanes.front(), 0);
        assert!(Arc::ptr_eq(core.config(), &selected));
        assert_eq!(core.lanes[1].environment().config.retries, Some(11));
        assert_eq!(core.lanes[1].resolved().retry.attempts, 11);
        assert_eq!(core.lanes[0].resolved().retry.attempts, 2);
        assert_eq!(
            core.settings().project(),
            Some(first.path().join(".pi/settings.toml").as_path())
        );
    }
}
