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

// The rows one construct occupies, and the row that names it: the single
// answer `block` and `outline` both reach through, so they cannot drift apart.
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

// The last row holding `node`, 0-based: stopping at column 0 means it
// stopped at the row boundary, not inside it — as with a swallowed newline.
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
        // `outer`, not `doc`: a `//!` header carries `doc` too but belongs to
        // the module, not the first item below — conflating them deletes it on edit.
        Mark::Outer(kind) => node.kind() == *kind && node.child_by_field_name("outer").is_some(),
        Mark::Opener(kind, opener) => {
            node.kind() == *kind
                && node
                    .utf8_text(src.as_bytes())
                    .is_ok_and(|text| text.starts_with(opener))
        }
    })
}

// Past the annotations to the thing they are about: Rust's attributes
// precede the item as siblings, Python's decorators are a wrapper's leading children.
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

// A node an address may name: what the parser couldn't read is not a
// construct, so `N*` over an error node would replace a span nobody looked at.
fn resolvable(node: Node) -> bool {
    node.is_named() && !node.is_error() && !node.is_missing()
}

// Whether `node` displaces `best` as the construct opening on their shared row.
fn widest(best: Option<Node>, node: Node, root: Node) -> bool {
    node != root
        && resolvable(node)
        && best.is_none_or(|b| node.end_byte() - node.start_byte() > b.end_byte() - b.start_byte())
}

/// Every row that opens a construct, in one parse: each mapped to that
/// construct's full extent, annotations included.
pub fn extents(lang: Lang, content: &str) -> HashMap<usize, (usize, usize)> {
    let Some(tree) = parse(lang, content) else {
        return HashMap::new();
    };
    let root = tree.root_node();
    // The widest node per row: `#[inline]` is a construct of its own and also
    // part of the function under it — the wider one is what a caller naming the row wants.
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

// `shown` is the nearest listed declaration's span, so a wrapper and what it
// wraps aren't listed twice — e.g. `export class C {}` shares one span.
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
        // The same `extent` a patch resolves this row through, so skeleton and
        // edit can't disagree; computed only for candidates, since most nodes aren't listed.
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
            // Undeclared nodes are still walked, since declarations often sit
            // inside an unlisted wrapper; only a span actually listed can suppress its duplicate.
            let shown = listed.map(|e| (e.start, e.end)).or(shown);
            let deeper = depth + usize::from(listed.is_some());
            visit(child, lang, src, lines, deeper, shown, out);
        }
    }
}
