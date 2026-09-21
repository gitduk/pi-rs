use std::collections::HashMap;
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

// The rows one construct occupies, and the row that names it.
//
// The single answer to "what is the thing at this row", so that `block` and
// `outline` cannot drift: both are this function, reached from a row and from
// a node respectively.
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

// The last row holding any of `node`, 0-based.
//
// A node stopping at column 0 stopped *at* that row's boundary, not inside it:
// a line comment swallows its own newline, and a markdown section closes where
// the next heading begins.
fn last_row(node: Node) -> usize {
    let end = node.end_position();
    if end.column == 0 && end.row > node.start_position().row {
        end.row - 1
    } else {
        end.row
    }
}

// Adjacent rows: an annotation binds to what starts on the row after it ends.
fn touches(a: Node, b: Node) -> bool {
    last_row(a) + 1 >= b.start_position().row
}

// Whether `node` documents or decorates whatever it touches.
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

// Past the annotations to the thing they are about.
//
// Two shapes, one walk each: Rust's attributes precede the item as siblings,
// Python's decorators are the leading children of a wrapper node.
fn subject<'t>(lang: Lang, node: Node<'t>, src: &str) -> Node<'t> {
    let mut n = node;
    while annotates(lang, n, src) {
        // Not onto what the parser could not read: the walk would carry on past
        // it to the next valid construct and report a span covering both.
        match n
            .next_named_sibling()
            .filter(|next| touches(n, *next) && resolvable(*next))
        {
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
            .and_then(|_| kids.find(|k| !annotates(lang, *k, src)))
            .filter(|inner| resolvable(*inner));
        match inner {
            Some(inner) => n = inner,
            None => return n,
        }
    }
}

// Out to the outermost node opening on the same row: `## Section` is a heading
// inside a section, and the section is what a reader means by it.
fn widen(node: Node) -> Node {
    let mut n = node;
    // The root opens on row 0, so climbing into it would make every line-1
    // construct the whole file.
    while let Some(p) = n.parent().filter(|p| p.parent().is_some()) {
        // Nor out into an error node: climbing through one hands back the span
        // the parser gave up on, which is not a construct anything may name.
        if p.start_position().row != n.start_position().row || !resolvable(p) {
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

// The annotation immediately above `node`, if one is touching it.
fn annotation_above<'t>(lang: Lang, node: Node<'t>, src: &str) -> Option<Node<'t>> {
    let prev = node.prev_named_sibling()?;
    (annotates(lang, prev, src) && touches(prev, node)).then_some(prev)
}

// A node an address may name. What the parser could not read is not a
// construct: `N*` over an error node replaces a span nobody looked at, and a
// view that printed its range would be inviting exactly that.
fn resolvable(node: Node) -> bool {
    node.is_named() && !node.is_error() && !node.is_missing()
}

// Whether `node` displaces `best` as the construct opening on their shared row.
//
// One spelling of the rule for both walks below. They prune differently — one
// row against every row — but a row's answer may not depend on which asked.
fn widest(best: Option<Node>, node: Node, root: Node) -> bool {
    node != root
        && resolvable(node)
        && best.is_none_or(|b| node.end_byte() - node.start_byte() > b.end_byte() - b.start_byte())
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
        if node.start_position().row == row && widest(best, node, root) {
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

/// What [`block`] answers for every row, in one parse: each row that opens a
/// construct, mapped to that construct's full extent.
pub fn extents(lang: Lang, content: &str) -> HashMap<usize, (usize, usize)> {
    let Some(tree) = parse(lang, content) else {
        return HashMap::new();
    };
    let root = tree.root_node();
    // `block` picks the largest node opening on a row; the same node is found
    // here by keeping the widest per row while the walk passes through.
    let mut best: HashMap<usize, Node> = HashMap::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let row = node.start_position().row;
        if widest(best.get(&row).copied(), node, root) {
            best.insert(row, node);
        }
        stack.extend(node.children(&mut cursor));
    }
    best.into_iter()
        .map(|(row, node)| {
            let e = extent(lang, node, content);
            (row + 1, (e.start, e.end))
        })
        .collect()
}

/// Every multi-row construct, as the row it opens on mapped to the row it
/// closes on. Keyed by the extent's start, annotations included — the number a
/// patch writes, which is not always the row that named it.
pub fn spans(lang: Lang, content: &str) -> HashMap<usize, usize> {
    extents(lang, content)
        .into_values()
        .filter(|(start, end)| end > start)
        .collect()
}

/// Every row tree-sitter could not parse, 1-based and ascending.
///
/// All of them, not just the first: an unbalanced brace makes the whole file
/// one error node opening on row 1, and the row worth reporting is the one
/// nearest what the caller touched.
pub fn error_rows(lang: Lang, content: &str) -> Vec<usize> {
    let Some(tree) = parse(lang, content) else {
        return Vec::new();
    };
    let root = tree.root_node();
    // The common answer is "none", and that one is a flag read, not a walk.
    if !root.has_error() {
        return Vec::new();
    }
    let mut cursor = root.walk();
    let mut stack = vec![root];
    let mut rows = Vec::new();
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            rows.push(node.start_position().row + 1);
            // An error node's children are whatever the grammar salvaged, not
            // further failures; descending would report the same break twice.
            continue;
        }
        if node.has_error() {
            stack.extend(node.children(&mut cursor));
        }
    }
    rows.sort_unstable();
    rows.dedup();
    rows
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

// `shown` is the span of the nearest declaration already listed, so a wrapper
// and the thing it wraps are not listed twice.
//
// `export class C {…}` is two declared nodes opening and closing on the same
// rows — the export and the class — and the reader wants one line, not two.
// Suppressing by span rather than by node kind keeps the language tables
// honest: `export_statement` really is the declaration when it wraps something
// anonymous, and says so by being the only node with that span.
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
