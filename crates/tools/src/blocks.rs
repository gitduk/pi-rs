/// Resolves hashline's `N*` through tree-sitter. Lives here rather than in
/// hashline so that crate stays a pure function of its inputs.
pub struct TreeSitter;

impl hashline::Blocks for TreeSitter {
    fn end_of(&self, path: &str, content: &str, line: usize) -> Option<usize> {
        let lang = syntax::Lang::of(path)?;
        syntax::block(lang, content, line).map(|(_, end)| end)
    }
}
