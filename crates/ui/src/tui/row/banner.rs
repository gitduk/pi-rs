//! A startup signature composed once per width, with one seed per process.

use std::cell::RefCell;
use std::collections::hash_map::{DefaultHasher, RandomState};
use std::hash::{BuildHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::LazyLock;

use ratatui::text::{Line, Span};

use crate::render::Paint;
use pi_store::icons;

static SEED: LazyLock<u64> = LazyLock::new(|| RandomState::new().hash_one("banner"));

// Including the leading 3. A version absent from this prefix is written out.
const DIGITS: &str = concat!(
    "31415926535897932384626433832795028841971693993751058209749445923078164062862089",
    "98628034825342117067982148086513282306647093844609550582231725359408128481117450",
    "28410270193852110555964462294895493038196442881097566593344612847564823378678316",
    "52712019091456485669234603486104543266482133936072602491412737245870066063155881",
    "74881520920962829254091715364367892590360011330530548820466521384146951941511609",
    "43305727036575959195309218611738193261179310511854807446237996274956735188575272",
    "48912279381830119491298336733624406566430860213949463952247371907021798609437027",
    "70539217176293176752384674818467669405132000568127145263560827785771342757789609",
    "17363717872146844090122495343014654958537105079227968925892354201995611212902196",
    "08640344181598136297747713099605187072113499999983729780499510597317328160963185",
    "95024459455346908302642522308253344685035261931188171010003137838752886587533208",
    "38142061717766914730359825349042875546873115956286388235378759375195778185778053",
    "2171226806613001927876611195909216420198938095257201065485863278",
);

pub(super) struct Banner {
    notes: Vec<Line<'static>>,
    top: std::cell::Cell<usize>,
    drawn: RefCell<Option<(usize, Line<'static>)>>,
}

impl Banner {
    pub(super) fn new(notes: &[String], paint: &Paint) -> Self {
        Self {
            notes: notes
                .iter()
                .map(|note| Line::from(paint.span(&paint.theme.muted, note)))
                .collect(),
            top: std::cell::Cell::new(2),
            drawn: RefCell::new(None),
        }
    }

    pub(super) fn len(&self) -> usize {
        1 + if self.notes.is_empty() {
            0
        } else {
            1 + self.notes.len()
        }
    }

    pub(super) fn prepare(&self, height: usize) -> bool {
        let top = (height / 8).min(3);
        if self.top.replace(top) == top {
            return false;
        }
        *self.drawn.borrow_mut() = None;
        true
    }

    pub(super) fn line(&self, i: usize, paint: &Paint, width: usize) -> Line<'static> {
        if i > 0 {
            return i
                .checked_sub(2)
                .and_then(|i| self.notes.get(i))
                .cloned()
                .unwrap_or_default();
        }
        let mut drawn = self.drawn.borrow_mut();
        if let Some((at, line)) = drawn.as_ref()
            && *at == width
        {
            return line.clone();
        }
        let mut line = draw(env!("CARGO_PKG_VERSION"), width, *SEED, paint);
        if self.top.get() > 0 {
            line.spans.insert(0, Span::raw("\n".repeat(self.top.get())));
        }
        *drawn = Some((width, line.clone()));
        line
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Space,
    Digit,
    Version,
    Mark,
    Rule,
}

#[derive(Clone, Copy)]
struct Cell {
    ch: char,
    role: Role,
}

impl Cell {
    const SPACE: Self = Self {
        ch: ' ',
        role: Role::Space,
    };
}

struct Source {
    cells: Vec<Cell>,
    parts: Vec<Range<usize>>,
}

impl Source {
    fn new(version: &str, width: usize) -> Option<Self> {
        let mut cursor = 0;
        let mut parts = Vec::new();
        let words: Vec<_> = version.split('.').collect();
        if words.len() != 3 {
            return None;
        }
        for word in words {
            if word.is_empty() || !word.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut start = cursor + DIGITS.get(cursor..)?.find(word)?;
            if start == 0 && word.len() > 1 {
                start = 1 + DIGITS[1..].find(word)?;
            }
            let end = start + word.len();
            let point = usize::from(start > 0);
            parts.push(start + point..end + point);
            cursor = end + 1;
        }
        let end = (cursor + 5)
            .max(width.saturating_sub(12).min(256))
            .min(DIGITS.len());
        let mut cells: Vec<_> = DIGITS[..end]
            .chars()
            .map(|ch| Cell {
                ch,
                role: Role::Digit,
            })
            .collect();
        cells.insert(
            1,
            Cell {
                ch: '.',
                role: Role::Digit,
            },
        );
        for part in &parts {
            for cell in &mut cells[part.clone()] {
                cell.role = Role::Version;
            }
        }
        cells.push(Cell {
            ch: '…',
            role: Role::Digit,
        });
        Some(Self { cells, parts })
    }

    fn can_cut(&self, at: usize) -> bool {
        self.parts
            .iter()
            .all(|part| at <= part.start || at >= part.end)
    }
}

#[derive(Clone, Copy)]
enum Axis {
    Across,
    Down,
}

#[derive(Clone, Copy)]
enum Align {
    Start,
    Center,
    End,
}

struct Layout(Vec<Vec<Cell>>);

impl Layout {
    fn text(text: &str, role: Role) -> Self {
        Self(vec![text.chars().map(|ch| Cell { ch, role }).collect()])
    }

    fn rule(length: usize, axis: Axis) -> Self {
        match axis {
            Axis::Across => Self::text(&"─".repeat(length), Role::Rule),
            Axis::Down => Self(vec![
                vec![Cell {
                    ch: '│',
                    role: Role::Rule
                }];
                length
            ]),
        }
    }

    fn width(&self) -> usize {
        self.0.iter().map(Vec::len).max().unwrap_or(0)
    }

    fn join(children: Vec<Self>, axis: Axis, gap: usize, align: Align) -> Self {
        let (width, height) = match axis {
            Axis::Across => (
                children.iter().map(Self::width).sum::<usize>() + gap * (children.len() - 1),
                children.iter().map(|c| c.0.len()).max().unwrap_or(0),
            ),
            Axis::Down => (
                children.iter().map(Self::width).max().unwrap_or(0),
                children.iter().map(|c| c.0.len()).sum::<usize>() + gap * (children.len() - 1),
            ),
        };
        let mut rows = vec![vec![Cell::SPACE; width]; height];
        let mut pos = 0;
        for child in children {
            let delta = match axis {
                Axis::Across => height - child.0.len(),
                Axis::Down => width - child.width(),
            };
            let offset = match align {
                Align::Start => 0,
                Align::Center => delta / 2,
                Align::End => delta,
            };
            let (dx, dy, length) = match axis {
                Axis::Across => (pos, offset, child.width()),
                Axis::Down => (offset, pos, child.0.len()),
            };
            for (y, row) in child.0.into_iter().enumerate() {
                rows[y + dy][dx..dx + row.len()].copy_from_slice(&row);
            }
            pos += length + gap;
        }
        Self(rows)
    }

    fn painted(self, paint: &Paint, width: usize) -> Line<'static> {
        let left = self
            .0
            .iter()
            .filter_map(|row| row.iter().position(|c| c.ch != ' '))
            .min()
            .unwrap_or(0);
        let right = self
            .0
            .iter()
            .filter_map(|row| row.iter().rposition(|c| c.ch != ' '))
            .max()
            .map_or(left, |i| i + 1);
        let indent = width.saturating_sub(right - left) / 2;
        let mut spans = Vec::new();
        for (y, row) in self.0.into_iter().enumerate() {
            if y > 0 {
                spans.push(Span::raw("\n"));
            }
            let end = row.iter().rposition(|c| c.ch != ' ').map_or(0, |i| i + 1);
            if end <= left {
                continue;
            }
            spans.push(Span::raw(" ".repeat(indent)));
            for cell in &row[left..end] {
                let style = match cell.role {
                    Role::Mark => &paint.theme.heading,
                    Role::Version => &paint.theme.strong,
                    Role::Digit | Role::Rule | Role::Space => &paint.theme.muted,
                };
                let span = paint.span(style, cell.ch.to_string());
                spans.push(if cell.role == Role::Version && paint.color {
                    ratatui::style::Stylize::bold(span)
                } else {
                    span
                });
            }
        }
        Line::from(spans)
    }
}

struct Choices {
    seed: u64,
    step: u64,
}

impl Choices {
    fn pick(&mut self, count: usize) -> usize {
        let mut hash = DefaultHasher::new();
        (self.seed, self.step).hash(&mut hash);
        self.step += 1;
        hash.finish() as usize % count
    }

    fn align(&mut self) -> Align {
        [Align::Start, Align::Center, Align::End][self.pick(3)]
    }
}

fn bind(body: Layout, width: usize, choices: &mut Choices) -> Layout {
    let axis = if body.width() + 3 > width || choices.pick(3) == 0 {
        Axis::Down
    } else {
        Axis::Across
    };
    let mut children = vec![Layout::text(icons::PI_MARK, Role::Mark)];
    if matches!(axis, Axis::Across) && body.width() + 9 <= width && choices.pick(2) == 0 {
        children.push(Layout::rule(2 + choices.pick(6), Axis::Across));
    }
    children.push(body);
    if choices.pick(4) == 0 {
        children.reverse();
    }
    let gap = if matches!(axis, Axis::Across) {
        2 + choices.pick(3)
    } else {
        0
    };
    Layout::join(children, axis, gap, choices.align())
}

fn candidate(source: &Source, width: usize, choices: &mut Choices) -> Option<Layout> {
    let n = source.cells.len();
    let minimum = n.div_ceil(width.saturating_sub(4).max(1)).clamp(1, 3);
    let count = minimum + choices.pick(4 - minimum);
    let mut cuts = vec![0];
    for i in 1..count {
        let target = n * i / count;
        let first = (cuts[i - 1] + 8).max(target.saturating_sub(5));
        let last = (target + 6).min(n.saturating_sub(8 * (count - i)));
        let options: Vec<_> = (first..last).filter(|&i| source.can_cut(i)).collect();
        if options.is_empty() {
            return None;
        }
        cuts.push(options[choices.pick(options.len())]);
    }
    cuts.push(n);
    let group = if width < 40 {
        0
    } else {
        [0, 0, 0, 5, 10][choices.pick(5)]
    };
    let stagger = if width < 40 {
        0
    } else {
        [0, 0, 2, 4][choices.pick(4)]
    };
    let mut children = Vec::new();
    for (i, pair) in cuts.windows(2).enumerate() {
        let (start, end) = (pair[0], pair[1]);
        let mut row = vec![Cell::SPACE; i * stagger];
        for at in start..end {
            if group > 0
                && at > start
                && (at - start) % group == 0
                && end - at >= 4
                && source.can_cut(at)
            {
                row.push(Cell::SPACE);
            }
            row.push(source.cells[at]);
        }
        children.push(Layout(vec![row]));
    }
    let mut body = Layout::join(children, Axis::Down, 0, choices.align());
    body = bind(body, width, choices);
    if choices.pick(4) != 0 {
        let axis = if width >= 40 && body.0.len() <= 3 && choices.pick(4) == 0 {
            Axis::Across
        } else {
            Axis::Down
        };
        let stroke = match axis {
            Axis::Across => Layout::rule(body.0.len(), Axis::Down),
            Axis::Down => Layout::rule(4 + choices.pick(body.width().max(5) - 3), Axis::Across),
        };
        let mut children = vec![body, stroke];
        if choices.pick(2) == 0 {
            children.reverse();
        }
        let gap = if matches!(axis, Axis::Across) {
            2
        } else {
            choices.pick(2)
        };
        body = Layout::join(children, axis, gap, choices.align());
    }
    let occupied = body
        .0
        .iter()
        .flatten()
        .filter(|c| c.role != Role::Space)
        .count();
    (body.width() <= width && body.0.len() <= 5 && occupied * 4 >= body.width() * body.0.len())
        .then_some(body)
}

fn draw(version: &str, width: usize, seed: u64, paint: &Paint) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let room = width - width / 10 * 2;
    if room >= 24
        && paint.color
        && let Some(source) = Source::new(version, room)
        && source.cells.len() <= room.saturating_mul(3)
    {
        let mut choices = Choices { seed, step: 0 };
        for _ in 0..64 {
            if let Some(layout) = candidate(&source, room, &mut choices) {
                return layout.painted(paint, width);
            }
        }
    }
    // Keep the full version readable when highlighting cannot convey it.
    let head = Layout::join(
        vec![
            Layout::text(icons::PI_MARK, Role::Mark),
            Layout::text(version, Role::Version),
        ],
        Axis::Across,
        1,
        Align::Start,
    );
    let mut rows = vec![head];
    if width >= 16 {
        let digits = &DIGITS[1..width.saturating_sub(3).min(32)];
        rows.push(Layout::text(
            &format!("3.{digits}{}", icons::ELLIPSIS),
            Role::Digit,
        ));
    }
    Layout::join(rows, Axis::Down, 0, Align::Start).painted(paint, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_layouts_preserve_pi_and_the_highlighted_version() {
        for width in [24, 32, 40, 60, 80, 120, 240] {
            let source = Source::new("2.9.10", width).expect("the version occurs in pi");
            for seed in 0..32 {
                let mut choices = Choices { seed, step: 0 };
                let layout = (0..64)
                    .find_map(|_| candidate(&source, width, &mut choices))
                    .unwrap_or_else(|| panic!("no layout at width {width}, seed {seed}"));
                assert!(layout.width() <= width, "width {width}, seed {seed}");
                let digits: String = layout
                    .0
                    .iter()
                    .flatten()
                    .filter(|c| matches!(c.role, Role::Digit | Role::Version))
                    .map(|c| c.ch)
                    .collect();
                assert_eq!(
                    digits,
                    source.cells.iter().map(|c| c.ch).collect::<String>()
                );
                let mut parts = Vec::new();
                for row in &layout.0 {
                    let mut run = String::new();
                    for cell in row.iter().chain(std::iter::once(&Cell::SPACE)) {
                        if cell.role == Role::Version {
                            run.push(cell.ch);
                        } else if !run.is_empty() {
                            parts.push(std::mem::take(&mut run));
                        }
                    }
                }
                assert_eq!(parts, ["2", "9", "10"], "width {width}, seed {seed}");
            }
        }
    }

    #[test]
    fn version_parts_are_matched_without_lighting_the_decimal_point() {
        for version in ["3.0.0", "31.4.15", "2.10.0", "1.0.0", "2.9.10"] {
            let source = Source::new(version, 80).expect("these version parts occur in pi");
            let parts: Vec<String> = source
                .parts
                .iter()
                .map(|part| source.cells[part.clone()].iter().map(|c| c.ch).collect())
                .collect();
            assert_eq!(parts, version.split('.').collect::<Vec<_>>(), "{version}");
            assert!(
                !source.parts.iter().any(|part| part.contains(&1)),
                "{version}"
            );
        }
    }
}
