use tree_sitter::{Node, Parser};

mod lang;
pub use lang::Lang;
use lang::Mark;

/// One entry in a file's skeleton.
///
/// `line..=end` is what a patch names to replace the whole thing, annotations
/// included. `text` is the row that identifies it — a later row than `line`
/// whenever something annotates it — trimmed, because `depth` is the structural
/// nesting and source indentation would double it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub line: usize,
    pub end: usize,
    pub depth: usize,
    pub text: String,
}

/// The rows one construct occupies, and the row that names it.
///
/// The single answer to "what is the thing at this row", so that `block` and
/// `outline` cannot drift: both are this function, reached from a row and from
/// a node respectively.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Extent {
    start: usize,
    end: usize,
    name: usize,
}

fn parse(lang: Lang, content: &str) -> Option<tree_sitter::Tree> {
    let mut p = Parser::new();
    p.set_language(&lang.grammar()).ok()?;
    p.parse(content, None)
}

/// The last row holding any of `node`, 0-based.
///
/// A node stopping at column 0 stopped *at* that row's boundary, not inside it:
/// a line comment swallows its own newline, and a markdown section closes where
/// the next heading begins.
fn last_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

/// Adjacent rows: an annotation binds to what starts on the row after it ends.
fn touches(a: Node, b: Node) -> bool {
    last_row(a) + 1 >= b.start_position().row
}

/// Whether `node` documents or decorates whatever it touches.
fn annotates(lang: Lang, node: Node, src: &str) -> bool {
    lang.annotations().iter().any(|mark| match mark {
        Mark::Kind(kind) => node.kind() == *kind,
        // `outer`, not `doc`: a `//!` header carries `doc` too and belongs to
        // the module around it, so absorbing it into the first item below
        // would delete the crate's own documentation on the first edit.
        Mark::Outer(kind) => node.kind() == *kind && node.child_by_field_name("outer").is_some(),
        Mark::Opener(kind, opener) => {
            node.kind() == *kind
                && node
                    .utf8_text(src.as_bytes())
                    .is_ok_and(|text| text.starts_with(opener))
        }
    })
}

/// Past the annotations to the thing they are about.
///
/// Two shapes, one walk each: Rust's attributes precede the item as siblings,
/// Python's decorators are the leading children of a wrapper node.
fn subject<'t>(lang: Lang, node: Node<'t>, src: &str) -> Node<'t> {
    let mut n = node;
    while annotates(lang, n, src) {
        match n.next_named_sibling().filter(|next| touches(n, *next)) {
            Some(next) => n = next,
            None => break,
        }
    }
    loop {
        // Led by an annotation, or it is not a wrapper; the same iterator
        // carries on from there rather than rescanning what already failed.
        let mut cursor = n.walk();
        let mut kids = n.named_children(&mut cursor);
        let inner = kids
            .next()
            .filter(|first| annotates(lang, *first, src))
            .and_then(|_| kids.find(|k| !annotates(lang, *k, src)));
        match inner {
            Some(inner) => n = inner,
            None => return n,
        }
    }
}

/// Out to the outermost node opening on the same row: `## Section` is a heading
/// inside a section, and the section is what a reader means by it.
fn widen(node: Node) -> Node {
    let mut n = node;
    // The root opens on row 0, so climbing into it would make every line-1
    // construct the whole file.
    while let Some(p) = n.parent().filter(|p| p.parent().is_some()) {
        if p.start_position().row != n.start_position().row {
            break;
        }
        n = p;
    }
    n
}

fn extent(lang: Lang, node: Node, src: &str) -> Extent {
    let subject = widen(subject(lang, node, src));
    let mut first = subject;
    while let Some(prev) = annotation_above(lang, first, src) {
        first = prev;
    }
    Extent {
        start: first.start_position().row + 1,
        end: last_row(subject) + 1,
        name: subject.start_position().row + 1,
    }
}

/// The annotation immediately above `node`, if one is touching it.
fn annotation_above<'t>(lang: Lang, node: Node<'t>, src: &str) -> Option<Node<'t>> {
    let prev = node.prev_named_sibling()?;
    (annotates(lang, prev, src) && touches(prev, node)).then_some(prev)
}

/// The construct that opens at `line`, as an inclusive 1-based range.
///
/// Resolves to the *largest* node starting on that row: `fn foo() {` belongs to
/// the whole function, not to its name. A row that opens nothing — a lone `}`,
/// a blank line — yields None rather than a guess. Annotations count as part of
/// what they annotate, so the range covers them whichever row is named.
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

    let e = extent(lang, best?, content);
    Some((e.start, e.end))
}

/// The file's declarations, in source order, nested by container.
pub fn outline(lang: Lang, content: &str) -> Vec<Item> {
    let Some(tree) = parse(lang, content) else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    let mut out = Vec::new();
    visit(tree.root_node(), lang, content, &lines, 0, None, &mut out);
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
    src: &str,
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
        // The same answer `block` gives for this row, so a skeleton entry and
        // the patch that acts on it can never name different things. Computed
        // only where it can be used: the walk passes through far more nodes
        // than it lists.
        let candidate = lang.declarations().contains(&kind);
        let span = candidate.then(|| extent(lang, child, src));
        let listed = span.filter(|e| shown != Some((e.start, e.end)));

        if let Some(Extent { start, end, name }) = listed {
            let text = lines.get(name - 1).map(|l| l.trim()).unwrap_or_default();
            out.push(Item {
                line: start,
                end,
                depth,
                text: text.to_string(),
            });
        }
        if container || !candidate {
            // Undeclared nodes are still walked: a declaration often sits inside
            // a wrapper the outline itself has no reason to show.
            // Only a span that was actually listed can suppress a duplicate of
            // itself. Carrying every span walked through would let a class's
            // body suppress the one method that shares its extent.
            let shown = listed.map(|e| (e.start, e.end)).or(shown);
            let deeper = depth + usize::from(listed.is_some());
            visit(child, lang, src, lines, deeper, shown, out);
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
@retry
@cache
def slow(n):
    return n

class A:
    def m(self):
        pass
";
        // Every row of the construct answers with the same range, and the row
        // that names it is the `def`, not whichever decorator came first.
        for row in 1..=3 {
            assert_eq!(block(Lang::Python, src, row), Some((1, 4)), "row {row}");
        }
        let items = outline(Lang::Python, src);
        // The listed range covers both decorators; the row that names it is the
        // `def`, which is what `text` comes from.
        assert_eq!((items[0].line, items[0].end), (1, 4));
        assert_eq!(items[0].text, "def slow(n):");
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
mod agreement {
    use super::*;

    /// The one property that made `block` and `outline` two implementations of
    /// the same idea worth unifying: every row a skeleton offers must resolve,
    /// through the other entry point, to exactly the range the skeleton showed.
    ///
    /// Run over this crate's own source, which carries doc comments, attributes
    /// and nesting, plus one fixture per remaining shape.
    #[test]
    fn every_listed_row_resolves_to_the_range_it_was_listed_with() {
        let cases: &[(Lang, &str)] = &[
            (Lang::Rust, include_str!("lib.rs")),
            (Lang::Rust, include_str!("lang.rs")),
            (
                Lang::Markdown,
                "# Top\n\nintro\n\n## A\n\nbody\n\n## B\n\ntail\n",
            ),
            (
                Lang::Python,
                "@a\n@b\ndef f():\n    pass\n\nclass C:\n    @property\n    def g(self):\n        return 1\n",
            ),
            (
                Lang::TypeScript,
                "/** doc */\nexport class S {\n  m(): void {}\n}\n\nexport default { a: 1 };\n",
            ),
            (
                Lang::Json,
                "{\n  \"a\": {\n    \"b\": 1\n  },\n  \"c\": 2\n}\n",
            ),
        ];
        let mut listed = 0;
        for (lang, src) in cases {
            for item in outline(*lang, src) {
                listed += 1;
                // Every row the skeleton offers, and every row it covers that
                // opens anything at all: none of them may resolve to a range
                // the skeleton did not show, or a patch written from the
                // skeleton acts on something else.
                for row in item.line..=item.end {
                    // A blank line or a lone brace opens nothing, which is an
                    // answer. Opening something outside the range is not.
                    if let Some((s, e)) = block(*lang, src, row) {
                        assert!(
                            s >= item.line && e <= item.end,
                            "{lang:?} row {row} of {:?} escaped to {s}-{e}",
                            item.text
                        );
                    }
                }
                assert_eq!(
                    block(*lang, src, item.line),
                    Some((item.line, item.end)),
                    "{lang:?} {:?}",
                    item.text
                );
            }
        }
        assert!(
            listed > 30,
            "the corpus stopped covering anything: {listed}"
        );
    }
}

#[cfg(test)]
mod attribute_tests {
    use super::*;

    const SRC: &str = "\
/// What f does.
#[inline]
#[must_use]
pub fn f() -> i32 {
    1
}
";

    #[test]
    fn every_row_of_a_construct_answers_with_the_same_range() {
        // Doc comment, both attributes, and the item itself: naming any of them
        // names the whole thing, so replacing f cannot orphan its own doc. Rows
        // inside the body open constructs of their own and are not this.
        for row in 1..=4 {
            assert_eq!(block(Lang::Rust, SRC, row), Some((1, 6)), "row {row}");
        }
    }

    #[test]
    fn the_doc_marker_is_what_attaches_a_comment_not_adjacency() {
        // A plain `//` touching a declaration is still a remark beside it.
        // Otherwise three hundred adjacent `// filler` lines would become part
        // of whatever they happen to sit above.
        let plain = "// about g\npub fn g() {}\n";
        assert_eq!(block(Lang::Rust, plain, 1), Some((1, 1)));
        assert_eq!(block(Lang::Rust, plain, 2), Some((2, 2)));

        // `///` is the language saying it documents what follows.
        let doc = "/// About g.\npub fn g() {}\n";
        assert_eq!(block(Lang::Rust, doc, 1), Some((1, 2)));
        assert_eq!(block(Lang::Rust, doc, 2), Some((1, 2)));

        // And a blank line detaches it again.
        let spaced = "/// Not about g.\n\npub fn g() {}\n";
        assert_eq!(block(Lang::Rust, spaced, 3), Some((3, 3)));
    }

    #[test]
    fn an_inner_doc_comment_belongs_to_the_module_not_the_item_below() {
        // `//!` and `/*!` carry the grammar's `doc` field like `///` does, and
        // document the thing around them. Folding a crate header into the first
        // declaration means the first edit to that declaration deletes it.
        for header in ["//! Crate docs.", "/*! Crate docs. */"] {
            let src = format!("{header}\npub fn bake() -> i32 {{\n    1\n}}\n");
            assert_eq!(block(Lang::Rust, &src, 2), Some((2, 4)), "{header}");
            assert_eq!(block(Lang::Rust, &src, 1), Some((1, 1)), "{header}");
            let items = outline(Lang::Rust, &src);
            assert_eq!((items[0].line, items[0].end), (2, 4), "{header}");
        }

        // `////` is not a doc comment to rustc and must not be one here.
        let four = "//// not a doc\npub fn f() {}\n";
        assert_eq!(block(Lang::Rust, four, 2), Some((2, 2)));
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
