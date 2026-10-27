use tool::Registry;

pub mod background;
pub mod bash;
mod blocks;
pub mod drift;
pub mod edit;
pub mod fetch;
pub mod glob;
pub mod grep;
mod parses;
pub mod process;
pub mod read;
mod rows;
pub mod rtk;
pub mod scripts;
mod syntax;
#[cfg(test)]
mod testing;
pub mod walk;
pub mod write;

/// Tools independent of their host. Shells, job controls and other stateful
/// tools are added by the owner of their lifetime.
pub fn builtin() -> Registry {
    Registry::new()
        .with(read::Read)
        .with(write::Write)
        .with(edit::Edit)
        .with(grep::Grep)
        .with(glob::Glob)
        .with(fetch::Fetch::default())
}
