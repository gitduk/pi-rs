use std::collections::BTreeMap;
use std::sync::Arc;

use brain::request::ToolDef;

use crate::Tool;

/// The active tool set. Ordering is stable so the tool block stays
/// prompt-cacheable across turns.
#[derive(Default, Clone)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Registry").field(&self.names()).finish()
    }
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The three tools an agent cannot work without.
    pub fn builtin() -> Self {
        Self::new()
            .with(crate::read::Read)
            .with(crate::write::Write)
            .with(crate::edit::Edit)
            .with(crate::grep::Grep)
            .with(crate::glob::Glob)
            .with(crate::bash::Bash)
    }

    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        self.tools.insert(tool.name().to_string(), Arc::new(tool));
        self
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn names(&self) -> Vec<&str> {
        self.tools.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// One tool gone, the rest untouched. `restrict` is the wrong shape for
    /// this: it consumes `self` while `names()` borrows it, so taking a set
    /// away means cloning every name to build the set that stays.
    ///
    /// A name that is not there is not an error — the caller is saying "not
    /// this one", and it already is not.
    pub fn without(mut self, name: &str) -> Self {
        self.tools.remove(name);
        self
    }

    /// Only the tools named survive. An unknown name is returned to the caller
    /// rather than silently dropped: a typo would otherwise disarm the agent.
    pub fn restrict(mut self, names: &[String]) -> Result<Self, String> {
        if let Some(bad) = names.iter().find(|n| !self.tools.contains_key(*n)) {
            return Err(bad.clone());
        }
        self.tools.retain(|k, _| names.contains(k));
        Ok(self)
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.schema(),
            })
            .collect()
    }
}
