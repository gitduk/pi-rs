/// Resolves hashline's `N*` through tree-sitter. Lives here rather than in
/// hashline so that crate stays a pure function of its inputs.
pub struct TreeSitter;

impl hashline::Blocks for TreeSitter {
    fn extent_of(&self, path: &str, content: &str, line: usize) -> Option<(usize, usize)> {
        syntax::block(syntax::Lang::of(path)?, content, line)
    }
}
