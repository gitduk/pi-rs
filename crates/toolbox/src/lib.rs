use tool::Registry;

pub mod bash;
mod blocks;
pub mod edit;
pub mod fetch;
pub mod glob;
pub mod grep;
pub mod judge;
mod parses;
pub mod process;
pub mod read;
mod rows;
pub mod rtk;
pub mod scripts;
mod syntax;
pub mod walk;
pub mod write;

/// The tools an agent cannot work without. `subagent` and `skill` are not
/// among them: both are hung on afterwards, by whoever can build them.
pub fn builtin() -> Registry {
    Registry::new()
        .with(read::Read)
        .with(write::Write)
        .with(edit::Edit)
        .with(grep::Grep)
        .with(glob::Glob)
        .with(bash::Bash)
        .with(fetch::Fetch::default())
}
