use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use llm::request::ToolDef;

use crate::{Tier, Tool};

/// Tools that can appear or change while a run works, read again each time
/// the set is asked: a tool written mid-run is offered on the next turn.
pub trait Source: Send + Sync {
    /// The tools as they stand now.
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
}

/// The active tool set. Ordering is stable so the tool block stays
/// prompt-cacheable across turns.
#[derive(Default, Clone)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
    // Read after `tools`, which win a name; held to the same cuts.
    sources: Vec<Arc<dyn Source>>,
    ceilings: Vec<Tier>,
    hidden: BTreeSet<String>,
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
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(tool);
                true
            }
            std::collections::btree_map::Entry::Occupied(_) => false,
        }
    }

    /// Read `source` whenever the set is asked. Its tools come after every
    /// offered one, so none of them can take an offered tool's name.
    pub fn read(&mut self, source: Arc<dyn Source>) {
        self.sources.push(source);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        match self.tools.get(name) {
            Some(tool) => Some(tool.clone()),
            None => self.sourced().remove(name),
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.all().into_keys().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.sourced().is_empty()
    }

    /// One tool gone, the rest untouched.
    ///
    /// A name that is not there is not an error — the caller is saying "not
    /// this one", and it already is not.
    pub fn without(mut self, name: &str) -> Self {
        self.tools.remove(name);
        self.hidden.insert(name.to_string());
        self
    }

    /// Only the tools a run capped at `ceiling` may call. A tool the model can
    /// see but not use costs it a refused turn before it finds another way.
    pub fn within(mut self, ceiling: Tier) -> Self {
        self.tools.retain(|_, t| t.tier().under(ceiling));
        self.ceilings.push(ceiling);
        self
    }

    pub fn defs(&self) -> Vec<ToolDef> {
        self.all()
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                input_schema: t.schema(),
            })
            .collect()
    }

    // What the sources hold now that no offered tool or cut rules out.
    fn sourced(&self) -> BTreeMap<String, Arc<dyn Tool>> {
        let mut out = BTreeMap::new();
        for tool in self.sources.iter().flat_map(|s| s.tools()) {
            let name = tool.name();
            if self.tools.contains_key(name)
                || self.hidden.contains(name)
                || !self.ceilings.iter().all(|&c| tool.tier().under(c))
            {
                continue;
            }
            out.entry(name.to_string()).or_insert(tool);
        }
        out
    }

    fn all(&self) -> BTreeMap<String, Arc<dyn Tool>> {
        let mut all = self.sourced();
        all.extend(self.tools.iter().map(|(n, t)| (n.clone(), t.clone())));
        all
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
        assert_eq!(r.names(), ["grep", "read"]);
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
        assert_eq!(r.clone().within(Tier::Read).names(), ["read"]);
        assert_eq!(r.clone().within(Tier::Write).names(), ["edit", "read"]);
        assert_eq!(r.clone().within(Tier::Net).names(), ["fetch", "read"]);
        assert_eq!(r.within(Tier::Exec).names().len(), 4);
    }

    struct Shelf(std::sync::Mutex<Vec<Arc<dyn Tool>>>);

    impl Source for Shelf {
        fn tools(&self) -> Vec<Arc<dyn Tool>> {
            self.0.lock().unwrap().clone()
        }
    }

    // A tool that shows up mid-run is offered from then on, but never over a
    // tool already offered, past the run's ceiling, or once taken away.
    #[test]
    fn a_source_is_read_each_time_and_held_to_the_same_cuts() {
        let shelf = Arc::new(Shelf(Default::default()));
        let mut r = Registry::new().with(Named("read", "built in", Tier::Read));
        r.read(shelf.clone());
        let r = r.within(Tier::Exec).without("gone");
        assert_eq!(r.names(), ["read"]);

        shelf.0.lock().unwrap().extend([
            Arc::new(Named("count", "script", Tier::Exec)) as Arc<dyn Tool>,
            Arc::new(Named("read", "script", Tier::Exec)),
            Arc::new(Named("gone", "script", Tier::Exec)),
        ]);
        assert_eq!(r.names(), ["count", "read"]);
        assert_eq!(r.get("read").unwrap().description(), "built in");
        assert!(r.get("count").is_some());
        assert!(r.get("gone").is_none());
        assert_eq!(r.clone().within(Tier::Read).names(), ["read"]);
    }
}
