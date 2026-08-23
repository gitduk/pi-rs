use tree_sitter::{Node, Parser};

mod lang;
pub use lang::Lang;

/// One entry in a file's skeleton: where the construct starts, where it ends,
/// and the single line a reader needs to recognize it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub line: usize,
    pub end: usize,
    pub depth: usize,
    pub text: String,
}

fn parse(lang: Lang, content: &str) -> Option<tree_sitter::Tree> {
    let mut p = Parser::new();
    p.set_language(&lang.grammar()).ok()?;
    p.parse(content, None)
}

/// The construct that opens at `line`, as an inclusive 1-based range.
///
/// Resolves to the *largest* node starting on that row: `fn foo() {` belongs to
/// the whole function, not to its name. A row that opens nothing — a lone `}`,
/// a blank line — yields None rather than a guess.
pub fn block(lang: Lang, content: &str, line: usize) -> Option<(usize, usize)> {
    let tree = parse(lang, content)?;
    let row = line.checked_sub(1)?;

    let mut best: Option<Node> = None;
    let mut cursor = tree.walk();
    let root = tree.root_node();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        // The root starts on row 0, so line 1 would otherwise resolve to the
        // entire file — a block op that replaces everything.
        if node != root
            && node.start_position().row == row
            && node.is_named()
            && best
                .is_none_or(|b| node.end_byte() - node.start_byte() > b.end_byte() - b.start_byte())
        {
            best = Some(node);
        }
        // A node ending before the row, or starting after it, holds nothing useful.
        if node.end_position().row >= row && node.start_position().row <= row {
            stack.extend(node.children(&mut cursor));
        }
    }

    let mut node = best?;
    // An attribute is a sibling of what it annotates, so the range has to run
    // through the declaration that follows it.
    while lang.attributes().contains(&node.kind()) {
        match node.next_named_sibling() {
            Some(next) => node = next,
            None => break,
        }
    }
    let end = node.end_position();
    // A node that stops at column 0 ends *at* that row's boundary, not inside
    // it: markdown sections close where the next heading begins.
    let end_line = if end.column == 0 && end.row > node.start_position().row {
        end.row
    } else {
        end.row + 1
    };
    Some((best?.start_position().row + 1, end_line))
}

/// The file's declarations, in source order, nested by container.
pub fn outline(lang: Lang, content: &str) -> Vec<Item> {
    let Some(tree) = parse(lang, content) else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    visit(tree.root_node(), lang, &lines, 0, None, &mut out);
    out
}

/// `shown` is the span of the nearest declaration already listed, so a wrapper
/// and the thing it wraps are not listed twice.
///
/// `export class C {…}` is two declared nodes opening and closing on the same
/// rows — the export and the class — and the reader wants one line, not two.
/// Suppressing by span rather than by node kind keeps the language tables
/// honest: `export_statement` really is the declaration when it wraps something
/// anonymous, and says so by being the only node with that span.
fn visit(
    node: Node,
    lang: Lang,
    lines: &[&str],
    depth: usize,
    shown: Option<(usize, usize)>,
    out: &mut Vec<Item>,
) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            continue;
        }
        let kind = child.kind();
        let container = lang.containers().contains(&kind);
        let line = child.start_position().row + 1;
        let end = child.end_position().row + 1;
        let declared = lang.declarations().contains(&kind) && shown != Some((line, end));

        if declared {
            let text = lines
                .get(line - 1)
                .map(|l| l.trim_end())
                .unwrap_or_default();
            out.push(Item {
                line,
                end,
                depth,
                text: text.to_string(),
            });
        }
        if container || !lang.declarations().contains(&kind) {
            // Undeclared nodes are still walked: a declaration often sits inside
            // a wrapper the outline itself has no reason to show.
            // Only a span that was actually listed can suppress a duplicate of
            // itself. Carrying every span walked through would let a class's
            // body suppress the one method that shares its extent.
            let shown = if declared { Some((line, end)) } else { shown };
            visit(
                child,
                lang,
                lines,
                depth + usize::from(declared),
                shown,
                out,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = "\
use std::fmt;

pub struct Point {
    x: i32,
}

impl Point {
    pub fn new(x: i32) -> Self {
        Self { x }
    }

    fn hidden(&self) {}
}
";

    #[test]
    fn detects_languages_by_extension() {
        assert_eq!(Lang::of("src/main.rs"), Some(Lang::Rust));
        assert_eq!(Lang::of("a/b.tsx"), Some(Lang::Tsx));
        assert_eq!(Lang::of("Cargo.toml"), None);
        assert_eq!(Lang::of("noext"), None);
    }

    #[test]
    fn an_outline_nests_methods_under_their_impl() {
        let items = outline(Lang::Rust, RUST);
        let got: Vec<_> = items
            .iter()
            .map(|i| (i.line, i.end, i.depth, i.text.trim()))
            .collect();
        assert_eq!(
            got,
            vec![
                (3, 5, 0, "pub struct Point {"),
                (7, 13, 0, "impl Point {"),
                (8, 10, 1, "pub fn new(x: i32) -> Self {"),
                (12, 12, 1, "fn hidden(&self) {}"),
            ]
        );
    }

    const TS: &str = "\
export interface Cfg {
  host: string;
}

export class Server {
  start(): void {}
}

export default { port: 8080 };
";

    #[test]
    fn a_wrapper_and_what_it_wraps_are_listed_once() {
        // `export class C` is two declared nodes over the same rows, and the
        // reader wants one line. The last of them still counts when it wraps
        // nothing nameable, which is the only way `export default { … }` gets
        // listed at all.
        let items = outline(Lang::TypeScript, TS);
        let got: Vec<_> = items
            .iter()
            .map(|i| (i.line, i.end, i.depth, i.text.trim()))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, 3, 0, "export interface Cfg {"),
                (5, 7, 0, "export class Server {"),
                (6, 6, 1, "start(): void {}"),
                (9, 9, 0, "export default { port: 8080 };"),
            ]
        );
    }

    #[test]
    fn a_method_sharing_its_parents_last_line_survives() {
        // The suppression key is the span actually listed, not every span
        // walked through: a class body has the same extent as its one method.
        let src = "class Dog:\n    def bark(self):\n        return 1\n";
        let items = outline(Lang::Python, src);
        let got: Vec<_> = items.iter().map(|i| (i.line, i.depth)).collect();
        assert_eq!(got, vec![(1, 0), (2, 1)]);
    }

    #[test]
    fn a_block_resolves_to_the_whole_construct_not_its_name() {
        // Line 8 opens `pub fn new`, which closes on line 10.
        assert_eq!(block(Lang::Rust, RUST, 8), Some((8, 10)));
        assert_eq!(block(Lang::Rust, RUST, 7), Some((7, 13)));
    }

    #[test]
    fn a_line_that_opens_nothing_resolves_to_nothing() {
        // Line 11 is blank and line 13 is a bare closing brace.
        assert_eq!(block(Lang::Rust, RUST, 11), None);
        assert_eq!(block(Lang::Rust, RUST, 13), None);
        assert_eq!(block(Lang::Rust, RUST, 999), None);
    }

    #[test]
    fn python_decorators_and_classes_resolve_as_one_construct() {
        let src = "\
@cache
def slow(n):
    return n

class A:
    def m(self):
        pass
";
        assert_eq!(block(Lang::Python, src, 1), Some((1, 3)));
        let items = outline(Lang::Python, src);
        assert_eq!(items[0].text, "@cache");
        assert!(items.iter().any(|i| i.text.trim() == "def m(self):"));
    }

    #[test]
    fn markdown_headings_span_their_section() {
        let src = "# Top\n\ntext\n\n## Sub\n\nmore\n";
        let items = outline(Lang::Markdown, src);
        let texts: Vec<_> = items.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(texts, vec!["# Top", "## Sub"]);
    }
}

#[cfg(test)]
mod attribute_tests {
    use super::*;

    const SRC: &str = "\
#[inline]
#[must_use]
pub fn f() -> i32 {
    1
}

// a standalone comment
pub fn g() {}
";

    #[test]
    fn a_block_pointed_at_an_attribute_runs_through_its_declaration() {
        assert_eq!(block(Lang::Rust, SRC, 1), Some((1, 5)));
        assert_eq!(block(Lang::Rust, SRC, 2), Some((2, 5)));
        assert_eq!(block(Lang::Rust, SRC, 3), Some((3, 5)));
    }

    #[test]
    fn a_standalone_comment_never_sweeps_the_declaration_below_it() {
        assert_eq!(block(Lang::Rust, SRC, 7), Some((7, 7)));
    }
}

#[cfg(test)]
mod markdown_tests {
    use super::*;

    const DOC: &str = "\
# Title

intro

## First

body of first

### Nested

deeper

## Second

body of second
";

    #[test]
    fn a_heading_block_covers_its_whole_section() {
        // `## First` runs through its nested subsection and stops at `## Second`.
        assert_eq!(block(Lang::Markdown, DOC, 5), Some((5, 12)));
        // The nested heading closes where its parent section does.
        assert_eq!(block(Lang::Markdown, DOC, 9), Some((9, 12)));
        assert_eq!(block(Lang::Markdown, DOC, 13), Some((13, 15)));
    }
}
