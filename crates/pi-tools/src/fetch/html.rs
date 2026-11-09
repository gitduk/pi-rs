//! A page as a reader sees it: markup dropped, block boundaries as line
//! breaks, entities resolved, whitespace tidied.

// Tags whose boundary a reader sees as a line break. Everything else is
// inline, and breaking there would split sentences mid-clause.
fn breaks(name: &str) -> bool {
    BREAKS.iter().any(|b| name.eq_ignore_ascii_case(b))
}

const BREAKS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "option",
    "p",
    "pre",
    "section",
    "table",
    "td",
    "th",
    "title",
    "tr",
    "ul",
];

// The tag's own name, borrowed and in its original case: lowercasing every
// tag into a fresh String would be thousands of allocations for nothing.
fn name_of(tag: &str) -> &str {
    let rest = tag.trim_start_matches('/');
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    &rest[..end]
}

// Where `needle` first appears in `haystack`, ignoring ASCII case. A byte
// offset and boundary, since every needle here starts with `<`.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| {
        h[i..i + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

// Where `name`'s closing tag begins. The name must end there too:
// `</scriptable-widget>` is not `</script>`, or skipped script leaks as prose.
fn close_of(haystack: &str, name: &str) -> Option<usize> {
    let needle = format!("</{name}");
    let mut base = 0;
    while let Some(n) = find_ci(&haystack[base..], &needle) {
        let at = base + n;
        base = at + needle.len();
        if !named_on(&haystack[base..]) {
            return Some(at);
        }
    }
    None
}

// Whether a tag name carries on into what follows, so `fetched` can be told
// from `fetchedly` and `script` from `scriptable`.
pub(super) fn named_on(rest: &str) -> bool {
    rest.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// A run of the document, and whether it came from inside a `<pre>` — where
// the whitespace is the content, and collapsing it rewrites the code.
struct Seg {
    pre: bool,
    text: String,
}

// HTML as the text a reader would see.
pub(super) fn detag(html: &str) -> String {
    let mut segs: Vec<Seg> = Vec::new();
    let mut out = String::with_capacity(html.len() / 2);
    let mut pre = false;
    let mut depth = 0usize;
    let mut rest = html;
    loop {
        let Some(lt) = rest.find('<') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];

        if let Some(after) = rest.strip_prefix("<!--") {
            rest = match after.find("-->") {
                Some(n) => &after[n + 3..],
                // An unterminated comment swallows the rest of the document,
                // which is what a browser does with it too.
                None => "",
            };
            continue;
        }
        // What follows `<` decides whether it opens a tag: a browser reads
        // `3 < 4` as text, and without this check everything to the next `>` vanishes.
        let opens = rest[1..]
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, '/' | '!' | '?'));
        if !opens {
            out.push('<');
            rest = &rest[1..];
            continue;
        }
        // A tag with no `>` is a document cut mid-tag — by the byte cap, most
        // likely. Emitting its source as prose is the one thing worse.
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[1..gt];
        let closing = tag.starts_with('/');
        let name = name_of(tag);
        rest = &rest[gt + 1..];

        if name.eq_ignore_ascii_case("pre") {
            // Nested `<pre>` is not a second region: only the outermost pair
            // opens and closes one, or an inner close would end it early.
            depth = if closing {
                depth.saturating_sub(1)
            } else {
                depth + 1
            };
            if depth <= 1 {
                segs.push(Seg {
                    pre,
                    text: std::mem::take(&mut out),
                });
                pre = !closing && depth == 1;
            }
            continue;
        }
        let hidden = name.eq_ignore_ascii_case("script") || name.eq_ignore_ascii_case("style");
        if hidden && !closing {
            // Past the close tag, not up to it: stopping at `</script` would
            // hand the same tag back here, hunting the document for a second.
            rest = match close_of(rest, name).map(|n| &rest[n..]) {
                Some(tag) => tag.find('>').map(|g| &tag[g + 1..]).unwrap_or_default(),
                None => "",
            };
            continue;
        }
        if hidden {
            // A close with no open before it. Dropping the tag is all it is
            // owed; treating it as an open swallows the rest of the document.
            continue;
        }
        // A tag's open and its close both mark the same boundary, and marking
        // it twice would put a blank line between every pair of list items.
        if breaks(name) && !out.trim_end_matches([' ', '\t']).ends_with('\n') {
            out.push('\n');
        }
    }
    segs.push(Seg { pre, text: out });

    segs.iter()
        .map(|seg| {
            let text = unescape(&seg.text);
            match seg.pre {
                true => text.trim_matches('\n').to_string(),
                false => tidy(&text),
            }
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// Named entities worth knowing by sight. The numeric forms are decoded
// arithmetically and are not listed.
const ENTITIES: &[(&str, &str)] = &[
    ("amp", "&"),
    ("lt", "<"),
    ("gt", ">"),
    ("quot", "\""),
    ("apos", "'"),
    ("nbsp", " "),
    ("mdash", "—"),
    ("ndash", "–"),
    ("hellip", "…"),
    ("lsquo", "‘"),
    ("rsquo", "’"),
    ("ldquo", "“"),
    ("rdquo", "”"),
    ("middot", "·"),
    ("bull", "•"),
    ("copy", "©"),
    ("reg", "®"),
    ("trade", "™"),
    ("times", "×"),
];

fn entity(body: &str) -> Option<String> {
    if let Some(digits) = body.strip_prefix('#') {
        let n = match digits.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => digits.parse().ok()?,
        };
        return char::from_u32(n).map(String::from);
    }
    ENTITIES
        .iter()
        .find(|(k, _)| *k == body)
        .map(|(_, v)| (*v).to_string())
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(amp) = rest.find('&') else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // Every entity worth decoding is short; a `;` further out belongs to
        // some later sentence, and a bare `&` is far commoner than either.
        let end = rest.as_bytes()[1..]
            .iter()
            .take(10)
            .position(|b| *b == b';')
            .map(|n| n + 1)
            .filter(|n| *n > 1);
        let Some(end) = end else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        match entity(&rest[1..end]) {
            Some(c) => out.push_str(&c),
            None => out.push_str(&rest[..=end]),
        }
        rest = &rest[end + 1..];
    }
    out
}

// Whitespace as a reader would see it: no trailing spaces, no runs of blanks
// inside a line, and never more than one blank line between paragraphs.
pub(super) fn tidy(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blanks = 0;
    for line in s.lines() {
        let mut words = line.split_whitespace().peekable();
        if words.peek().is_none() {
            blanks += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blanks > 0 {
                out.push('\n');
            }
        }
        blanks = 0;
        let mut first = true;
        for w in words {
            if !first {
                out.push(' ');
            }
            out.push_str(w);
            first = false;
        }
    }
    out
}
