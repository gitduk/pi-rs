/// Resolves hashline's `whole_block` anchors through tree-sitter. Lives here
/// rather than in hashline so that crate stays a pure function of its inputs.
pub struct TreeSitter;

impl hashline::Blocks for TreeSitter {
    fn extent_of(&self, path: &str, content: &str, line: usize) -> Option<(usize, usize)> {
        syntax::block(syntax::Lang::of(path)?, content, line)
    }

    // One walk of the file, where the default would resolve every opening on
    // its own: a refusal naming many candidates would parse once per candidate.
    fn extents(&self, path: &str, content: &str) -> Vec<(usize, usize)> {
        let Some(lang) = syntax::Lang::of(path) else {
            return Vec::new();
        };
        let mut rows: Vec<(usize, usize)> = syntax::extents(lang, content).into_values().collect();
        rows.sort_unstable();
        rows
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
