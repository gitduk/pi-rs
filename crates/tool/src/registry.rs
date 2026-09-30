use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::sync::Arc;

use llm::request::ToolDef;

use crate::{Tier, Tool};

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

    /// Add `tool` to a set being built. Panics on a name already in the set:
    /// here that is a mistake in the set's own definition, not a conflict.
    pub fn with(mut self, tool: impl Tool + 'static) -> Self {
        let name = tool.name().to_string();
        assert!(
            self.offer(Arc::new(tool)),
            "tool {name} is registered twice"
        );
        self
    }

    /// Add `tool` unless its name is taken, and say whether it went in. The
    /// first to claim a name keeps it, whichever source offered it.
    pub fn offer(&mut self, tool: Arc<dyn Tool>) -> bool {
        match self.tools.entry(tool.name().to_string()) {
            Entry::Vacant(slot) => {
                slot.insert(tool);
                true
            }
            Entry::Occupied(_) => false,
        }
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

    /// Only the tools a run capped at `ceiling` may call. A tool the model can
    /// see but not use costs it a refused turn before it finds another way.
    pub fn within(mut self, ceiling: Tier) -> Self {
        self.tools.retain(|_, t| t.tier().under(ceiling));
        self
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Ctx, ToolError, ToolOutput};
    use serde_json::Value;

    struct Named(&'static str, &'static str, Tier);

    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            self.1
        }
        fn schema(&self) -> Value {
            Value::Null
        }
        fn tier(&self) -> Tier {
            self.2
        }
        async fn execute(&self, _: Value, _: &Ctx) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(""))
        }
    }

    #[test]
    fn the_first_to_claim_a_name_keeps_it() {
        let mut r = Registry::new();
        assert!(r.offer(Arc::new(Named("read", "first", Tier::Read))));
        assert!(!r.offer(Arc::new(Named("read", "second", Tier::Read))));
        assert!(r.offer(Arc::new(Named("grep", "other", Tier::Read))));
        assert_eq!(r.get("read").unwrap().description(), "first");
        assert_eq!(r.names(), vec!["grep", "read"]);
    }

    // Hidden rather than refused: a tool the model never sees is one it never
    // spends a turn on, and a wrong cut here is invisible until it does.
    #[test]
    fn only_tools_under_the_ceiling_stay() {
        let r = Registry::new()
            .with(Named("read", "", Tier::Read))
            .with(Named("edit", "", Tier::Write))
            .with(Named("bash", "", Tier::Exec))
            .with(Named("fetch", "", Tier::Net));
        assert_eq!(r.clone().within(Tier::Read).names(), vec!["read"]);
        assert_eq!(r.clone().within(Tier::Write).names(), vec!["edit", "read"]);
        assert_eq!(r.clone().within(Tier::Net).names(), vec!["fetch", "read"]);
        assert_eq!(r.within(Tier::Exec).names().len(), 4);
    }
}
