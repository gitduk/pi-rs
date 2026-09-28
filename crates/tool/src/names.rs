//! The names the built-in tools answer to. A tool is found by name in the
//! transcript and on the wire, and code outside its crate that treats one
//! specially names it from here, so a rename cannot leave a caller behind.

pub const BASH: &str = "bash";
pub const EDIT: &str = "edit";
pub const FETCH: &str = "fetch";
pub const GLOB: &str = "glob";
pub const GREP: &str = "grep";
pub const JUDGE: &str = "judge";
pub const READ: &str = "read";
pub const SKILL: &str = "skill";
pub const SUBAGENT: &str = "subagent";
pub const WRITE: &str = "write";
