//! The layout the bar is drawn from: `~/.pi/bar.rs` prints one, and without
//! it the default below stands.
//!
//! The script says where things go; what a part reads as is filled by the
//! surface each frame, so a `/model` switch shows at once, not after rerun.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::store::status::Segment;

/// A script's output read as a layout. A refusal names where it went wrong —
/// `lines[0].left[2]` — and why, rather than that nothing matched.
pub fn parse(json: &str) -> Result<Layout, String> {
    let de = &mut serde_json::Deserializer::from_str(json);
    serde_path_to_error::deserialize(de).map_err(|e| match e.path().to_string().as_str() {
        "." => format!("not a layout: {}", e.inner()),
        at => format!("not a layout: {at}: {}", e.inner()),
    })
}

/// The bar's lines, top to bottom, each one row whatever it has to say: a row
/// that came and went with a flash would move the transcript above it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub lines: Vec<Line>,
    /// Seconds until the script reruns unprompted, for state outside pi.
    /// Absent, only a change in pi's own state runs it.
    #[serde(default)]
    pub refresh: Option<f64>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            lines: vec![Line {
                left: vec![Item::Part(Part::Tabs), Item::Part(Part::Model)],
                right: vec![Item::Part(Part::Flash)],
                sep: None,
            }],
            refresh: None,
        }
    }
}

/// One row: what reads from its left edge, and what stands against its right.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    #[serde(default)]
    pub left: Vec<Item>,
    #[serde(default)]
    pub right: Vec<Item>,
    /// What stands between two items on a side. Absent, a muted ` · `.
    #[serde(default)]
    pub sep: Option<Text>,
}

/// A part pi fills in — a bare name, or `{"part", "style"}` to colour it —
/// the script's own text, or texts joined with no separator between them.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Part(Part),
    Text(Text),
    Group(Group),
    Styled(Styled),
}

/// The parts pi keeps current itself. One with nothing to say drops out,
/// and the separator before it with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// The checkouts, folded to the room the rest of the row leaves.
    Tabs,
    /// The answer to the last keypress, for a second.
    Flash,
    Model,
    /// Absent while thinking is off.
    Effort,
    Tier,
    /// A status line part — `ctx`, `cost`, `elapsed` … — drawn as the status
    /// line draws it, from the run in flight or the one that last ended.
    Status(Segment),
}

// Pi's own names; any other is a status line part's.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Own {
    Tabs,
    Flash,
    Model,
    Effort,
    Tier,
}

impl<'de> Deserialize<'de> for Part {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let own = match Own::deserialize(v.clone()) {
            Ok(own) => own,
            Err(not_own) => {
                return Segment::deserialize(v)
                    .map(Part::Status)
                    .map_err(|not_status| {
                        D::Error::custom(format!("{not_own}; nor a status part: {not_status}"))
                    });
            }
        };
        Ok(match own {
            Own::Tabs => Part::Tabs,
            Own::Flash => Part::Flash,
            Own::Model => Part::Model,
            Own::Effort => Part::Effort,
            Own::Tier => Part::Tier,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Text {
    pub text: String,
    #[serde(default)]
    pub style: Look,
}

/// A theme name, or a style written out as the theme file writes one:
/// `"34"`, `"#5c9cf5"`, `{"color": "36", "sgr": ["bold"]}`.
#[derive(Debug, Clone, PartialEq)]
pub enum Look {
    Tone(Tone),
    Style(crate::store::theme::Style),
}

impl Default for Look {
    fn default() -> Self {
        Look::Tone(Tone::Muted)
    }
}

// By hand rather than untagged: an untagged miss says only that no variant
// matched, where the key that was written says which one was meant.
impl<'de> Deserialize<'de> for Item {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let item = match &v {
            Value::String(_) => Part::deserialize(v).map(Item::Part),
            Value::Object(m) if m.contains_key("part") => Styled::deserialize(v).map(Item::Styled),
            Value::Object(m) if m.contains_key("group") => Group::deserialize(v).map(Item::Group),
            Value::Object(m) if m.contains_key("text") => Text::deserialize(v).map(Item::Text),
            _ => Err(serde_json::Error::custom(
                "an item is a part's name, or an object with `part`, `text` or `group`",
            )),
        };
        item.map_err(D::Error::custom)
    }
}

impl<'de> Deserialize<'de> for Look {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let style = |v| crate::store::theme::Style::deserialize(v).map(Look::Style);
        let look = match &v {
            Value::String(_) => match Tone::deserialize(v.clone()) {
                Ok(tone) => Ok(Look::Tone(tone)),
                Err(not_tone) => style(v).map_err(|not_style| {
                    serde_json::Error::custom(format!("{not_tone}, nor a style: {not_style}"))
                }),
            },
            _ => style(v),
        };
        look.map_err(D::Error::custom)
    }
}

/// A part in a colour of the script's choosing. The checkouts keep their
/// own: theirs say which one is in front and how each last run ended.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Styled {
    pub part: Part,
    #[serde(default)]
    pub style: Look,
}

/// Items drawn with nothing between them: a label and its part, say. Gone
/// whole when every part in it has nothing to say, so no label stands alone.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    #[serde(deserialize_with = "flat")]
    pub group: Vec<Item>,
}

// The checkouts fold to the room a row leaves, which a group cannot give them.
fn flat<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Item>, D::Error> {
    let items = Vec::<Item>::deserialize(d)?;
    let nested = items.iter().any(|item| match item {
        Item::Group(_) | Item::Part(Part::Tabs) => true,
        Item::Styled(s) => s.part == Part::Tabs,
        Item::Text(_) | Item::Part(_) => false,
    });
    match nested {
        true => Err(D::Error::custom(
            "a group holds texts and parts, but not `tabs` or another group",
        )),
        false => Ok(items),
    }
}

/// The theme's names for a colour, so script text follows the theme rather
/// than carrying escapes of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    #[default]
    Muted,
    Heading,
    Emphasis,
    Code,
    Input,
    Ok,
    Err,
    /// The colours a diff's added and removed lines wear.
    Add,
    Del,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_are_bare_names_and_text_carries_a_tone() {
        let layout: Layout = serde_json::from_str(
            r#"{"lines":[{"left":["tabs",{"text":"main*","style":"ok"}],"right":["flash"]},{}],
                "refresh":2}"#,
        )
        .unwrap();
        assert_eq!(layout.lines.len(), 2);
        assert_eq!(
            layout.lines[0].left[1],
            Item::Text(Text {
                text: "main*".into(),
                style: Look::Tone(Tone::Ok)
            })
        );
        assert_eq!(layout.lines[1], Line::default());
        assert_eq!(layout.refresh, Some(2.0));
    }

    #[test]
    fn a_group_holds_texts_each_with_its_own_tone() {
        let line: Line = serde_json::from_str(
            r#"{"left":[{"group":[{"text":"+5","style":"ok"},{"text":"/"},{"text":"-1","style":"err"}]}]}"#,
        )
        .unwrap();
        let Item::Group(g) = &line.left[0] else {
            panic!("{line:?}");
        };
        assert_eq!(g.group.len(), 3);
        assert!(matches!(&g.group[1], Item::Text(t) if t.style == Look::default()));
    }

    #[test]
    fn status_parts_are_parts_and_a_group_may_hold_them() {
        let line: Line = serde_json::from_str(
            r#"{"left":["ctx",{"group":[{"text":"$"},{"part":"cost","style":"ok"}]}]}"#,
        )
        .unwrap();
        assert_eq!(line.left[0], Item::Part(Part::Status(Segment::Ctx)));
        let Item::Group(g) = &line.left[1] else {
            panic!("{line:?}");
        };
        assert!(matches!(&g.group[1], Item::Styled(s) if s.part == Part::Status(Segment::Cost)));

        let tabs = parse(r#"{"lines":[{"left":[{"group":["tabs"]}]}]}"#).unwrap_err();
        assert!(tabs.contains("not `tabs`"), "{tabs}");
        let clock = parse(r#"{"lines":[{"left":["clock"]}]}"#).unwrap_err();
        assert!(clock.contains("nor a status part"), "{clock}");
    }

    #[test]
    fn a_style_may_be_written_out_as_the_theme_writes_one() {
        let t: Text = serde_json::from_str(r#"{"text":"x","style":"34"}"#).unwrap();
        assert!(matches!(t.style, Look::Style(_)), "{t:?}");
        let t: Text = serde_json::from_str(r#"{"text":"x","style":{"sgr":["dim"]}}"#).unwrap();
        assert!(matches!(t.style, Look::Style(_)), "{t:?}");
        let bad = r#"{"text":"x","style":"blurple"}"#;
        assert!(serde_json::from_str::<Text>(bad).is_err());
    }

    #[test]
    fn a_part_may_carry_a_style_and_a_line_its_separator() {
        let line: Line = serde_json::from_str(
            r#"{"left":[{"part":"model","style":"36"},"tier"],"sep":{"text":"  "}}"#,
        )
        .unwrap();
        assert!(
            matches!(&line.left[0], Item::Styled(s) if s.part == Part::Model),
            "{line:?}"
        );
        assert_eq!(line.sep.map(|s| s.text), Some("  ".into()));
    }

    #[test]
    fn an_unknown_part_is_refused() {
        let bad = r#"{"lines":[{"left":["clock"]}]}"#;
        assert!(serde_json::from_str::<Layout>(bad).is_err());
    }

    #[test]
    fn a_refusal_says_where_and_why() {
        let bad = r#"{"lines":[{"left":["tabs",{"part":"model","style":{"color":"add"}}]}]}"#;
        let why = parse(bad).unwrap_err();
        assert!(why.contains("lines[0].left[1]"), "{why}");
        assert!(why.contains("`add` is not a colour"), "{why}");

        let why = parse(r#"{"lines":[{"left":[{"text":"x","style":"blurple"}]}]}"#).unwrap_err();
        assert!(
            why.contains("blurple") && why.contains("nor a style"),
            "{why}"
        );
    }
}
