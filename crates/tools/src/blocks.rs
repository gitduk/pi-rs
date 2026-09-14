/// Resolves hashline's `*` construct scopes through tree-sitter. Lives here
/// rather than in hashline so that crate stays a pure function of its inputs.
pub struct TreeSitter;

impl hashline::Blocks for TreeSitter {
    fn extent_of(&self, path: &str, content: &str, line: usize) -> Option<(usize, usize)> {
        syntax::block(syntax::Lang::of(path)?, content, line)
    }

    fn openings(&self, path: &str, content: &str) -> Vec<usize> {
        let Some(lang) = syntax::Lang::of(path) else {
            return Vec::new();
        };
        let mut rows: Vec<usize> = syntax::extents(lang, content).keys().copied().collect();
        rows.sort_unstable();
        rows
    }
}
