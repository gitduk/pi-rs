//! The layout the statusline is drawn from: `~/.pi/statusline.json` holds one
//! the `statusline` tool wrote, and without it the default below stands.
//!
//! The layout says where things go; what a field reads as is filled by the
//! surface each frame, so a model switch shows at once.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// Where the layout the model set waits between runs: one file for every
/// checkout, beside the other state in the pi root.
pub fn path() -> Option<std::path::PathBuf> {
    crate::dir().map(|d| d.join("statusline.json"))
}

/// What a row last showed of the parts nothing else holds — a resumed session
/// paints them before anything runs. Its file is `statusline.json` inside the
/// session's own directory: the layout's name, a different shape.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
pub struct Shown {
    /// Percent of the last run's prompt tokens read from cache, floored.
    #[serde(default)]
    pub cached: Option<u64>,
    /// What the last run cost, in dollars. Zero is an unpriced model.
    #[serde(default)]
    pub cost: f64,
    /// What the last run's final request occupied against the window.
    #[serde(default)]
    pub ctx: Option<(usize, usize)>,
}

impl Shown {
    /// What the session filed in `dir` left, if anything ever did.
    pub fn read(dir: &std::path::Path) -> Option<Self> {
        let text = std::fs::read_to_string(dir.join(SHOWN)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Keep these for the session's next run.
    pub fn write(&self, dir: &std::path::Path) -> std::io::Result<()> {
        let text = serde_json::to_string(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::create_dir_all(dir)?;
        tool::state::write_private(&dir.join(SHOWN), text.as_bytes())
    }
}

const SHOWN: &str = "statusline.json";

/// The file as it is on disk: its text, and when it last changed.
pub fn read_text() -> Option<(String, std::time::SystemTime)> {
    let mut file = std::fs::File::open(path()?).ok()?;
    let at = file.metadata().ok()?.modified().ok()?;
    let mut text = String::new();
    std::io::Read::read_to_string(&mut file, &mut text).ok()?;
    Some((text, at))
}

/// Replace it whole: a reader racing the write sees the old file or the new
/// one, never half of either.
pub fn write_text(json: &str) -> std::io::Result<()> {
    let path = path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no pi root to write in")
    })?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let temp = path.with_extension("json.tmp");
    std::fs::write(&temp, json)?;
    std::fs::rename(&temp, &path)
}

/// Back to the built-in default: the file goes, nothing is left behind.
pub fn clear() -> std::io::Result<()> {
    match path() {
        Some(path) if path.exists() => std::fs::remove_file(path),
        _ => Ok(()),
    }
}

/// A script's output read as a layout. A refusal names where it went wrong —
/// `lines[0].left[2]` — and why, rather than that nothing matched.
pub fn parse(json: &str) -> Result<Layout, String> {
    let de = &mut serde_json::Deserializer::from_str(json);
    serde_path_to_error::deserialize(de).map_err(|e| match e.path().to_string().as_str() {
        "." => format!("not a layout: {}", e.inner()),
        at => format!("not a layout: {at}: {}", e.inner()),
    })
}

/// The statusline's lines, top to bottom, each one row whatever it has to
/// say: a row that came and went with a flash would move the transcript above.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub lines: Vec<Line>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            lines: vec![Line {
                place: Place::Bottom,
                left: vec![
                    Item::Field(Field::Tabs),
                    Item::Field(Field::Model),
                    Item::Styled(Styled {
                        field: Field::Spent,
                        style: Look::Tone(Tone::Muted),
                    }),
                ],
                right: vec![
                    Item::Styled(Styled {
                        field: Field::Update,
                        style: Look::Tone(Tone::Emphasis),
                    }),
                    Item::Field(Field::Flash),
                ],
                sep: None,
            }],
        }
    }
}

impl Layout {
    /// How many rows this layout takes in `place`, said or not: a row that
    /// comes and goes would move the panel under it.
    pub fn rows(&self, place: Place) -> usize {
        self.lines.iter().filter(|line| line.place == place).count()
    }

    /// Whether any row draws `field`: the work behind it is worth doing only
    /// then.
    pub fn uses(&self, field: Field) -> bool {
        self.items().any(|item| match item {
            Item::Field(f) => *f == field,
            Item::Styled(s) => s.field == field,
            _ => false,
        })
    }

    /// Whether any row draws a script: what a script reads is worth reading
    /// even when no field shows it.
    pub fn scripted(&self) -> bool {
        self.items().any(|item| matches!(item, Item::Script(_)))
    }

    /// Every item in the layout, groups opened, in drawing order.
    pub fn items(&self) -> impl Iterator<Item = &Item> {
        fn walk<'a>(items: &'a [Item], out: &mut Vec<&'a Item>) {
            for item in items {
                out.push(item);
                if let Item::Group(g) = item {
                    walk(&g.group, out);
                }
            }
        }
        let mut out = Vec::new();
        for line in &self.lines {
            walk(&line.left, &mut out);
            walk(&line.right, &mut out);
        }
        out.into_iter()
    }
}

/// Whether a run under `ceiling` may start an outside command: a script holds
/// the same power the shell tool does, so only a ceiling that offers the shell
/// offers it.
pub fn may_run_scripts(ceiling: tool::Tier) -> bool {
    tool::Tier::Exec.under(ceiling)
}

/// Where a checkout stands in git: the branch it is on, then the lines its
/// worktree has added and removed against `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Git {
    pub branch: String,
    pub added: u64,
    pub removed: u64,
}

/// A value a program outside pi prints: one line, on a cadence of its own.
/// `command` runs through the shell; `name` runs the file of that name under
/// `~/.pi/statusline.d`, which a person can read before it runs.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    /// A line for the shell to run.
    #[serde(default)]
    pub command: Option<String>,
    /// The file under `~/.pi/statusline.d` to run.
    #[serde(default)]
    pub name: Option<String>,
    /// Seconds between runs; the floor is `EVERY_LEAST`.
    #[serde(default = "every_default")]
    pub every: u64,
    /// Seconds one run may take before it is killed; the script says how long
    /// it is allowed, so a slow one is not mistaken for a hung one.
    #[serde(default = "timeout_default")]
    pub timeout: u64,
    /// What stands in its place until a run has answered.
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub style: Look,
}

/// The shortest cadence a script may ask for: one run a second, not one a
/// frame.
pub const EVERY_LEAST: u64 = 1;
/// The longest a run may hold its cell: a row is not a job's home.
pub const TIMEOUT_MOST: u64 = 300;
fn every_default() -> u64 {
    60
}
fn timeout_default() -> u64 {
    15
}

impl Script {
    /// Whether it can run at all: one of `command` or `name`, and a cadence
    /// at or above the floor.
    pub fn validate(&self) -> Result<(), String> {
        let shape = match (&self.command, &self.name) {
            (Some(command), None) if !command.trim().is_empty() => Ok(()),
            (Some(_), Some(_)) => Err("a script takes `command` or `name`, not both".to_string()),
            (Some(_), None) => Err("a script's `command` is empty".to_string()),
            (None, Some(name)) => script_path(name).map(|_| ()),
            (None, None) => Err("a script takes `command` or `name`".to_string()),
        };
        shape?;
        match self.every >= EVERY_LEAST {
            true => {}
            false => {
                return Err(format!(
                    "`every` is at least {EVERY_LEAST} seconds: this one asked for {}",
                    self.every
                ));
            }
        }
        match self.timeout <= TIMEOUT_MOST {
            true => Ok(()),
            false => Err(format!(
                "`timeout` is at most {TIMEOUT_MOST} seconds: this one asked for {}",
                self.timeout
            )),
        }
    }

    /// What a line calls it: the command, or the name it was given.
    pub fn label(&self) -> String {
        self.command
            .clone()
            .or_else(|| self.name.clone())
            .unwrap_or_default()
    }

    /// What runs: the command, or the file a name stands for.
    pub fn runnable(&self) -> Result<Runnable, String> {
        self.validate()?;
        match (&self.command, &self.name) {
            (Some(command), None) if !command.trim().is_empty() => {
                Ok(Runnable::Shell(command.clone()))
            }
            (Some(_), Some(_)) => Err("a script takes `command` or `name`, not both".into()),
            (None, Some(name)) => Ok(Runnable::File(script_path(name)?)),
            (None, None) => Err("a script takes `command` or `name`".into()),
            (Some(_), None) => Err("a script's `command` is empty".into()),
        }
    }

    /// Where it stands until it has answered: its own word, or a bare one.
    pub fn waiting(&self) -> &str {
        self.placeholder.as_deref().unwrap_or("…")
    }
}

/// What one script run executes, owned so a run can outlive the layout it
/// came from.
pub enum Runnable {
    Shell(String),
    File(std::path::PathBuf),
}

/// Where a named script lives: `~/.pi/statusline.d/<name>`, and nowhere else.
pub fn script_path(name: &str) -> Result<std::path::PathBuf, String> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.starts_with('-')
        || name.contains(['/', '\\'])
    {
        return Err(format!(
            "`{name}` is no script name: one file under ~/.pi/statusline.d, no path in it"
        ));
    }
    // The name alone is checked here; whether the file is there is the run's
    // business, so a script not written yet costs its own field, not the row.
    let dir = crate::dir()
        .ok_or("no pi root to look in")?
        .join("statusline.d");
    Ok(dir.join(name))
}

/// Which side of the input line a row is drawn on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Place {
    /// Under the input line, where the row has always stood.
    #[default]
    Bottom,
    /// Over the input line, between it and the conversation above.
    Top,
}

/// One row: what reads from its left edge, and what stands against its right.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    #[serde(default)]
    pub place: Place,
    #[serde(default)]
    pub left: Vec<Item>,
    #[serde(default)]
    pub right: Vec<Item>,
    /// What stands between two items on a side. Absent, a muted ` · `.
    #[serde(default)]
    pub sep: Option<Text>,
}

/// One thing a row draws: a field pi fills in — a bare name, or
/// `{"field", "style"}` to colour it — fixed text, texts joined with no
/// separator between them, or a value an outside script prints.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Field(Field),
    Text(Text),
    Group(Group),
    Styled(Styled),
    Script(Script),
}

/// What pi keeps current itself. One with nothing to say drops out, and the
/// separator before it with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Field {
    /// The checkouts, folded to the room the rest of the row leaves; a lone
    /// checkout draws nothing.
    Tabs,
    /// The answer to the last keypress, for a second.
    Flash,
    /// The model, as its endpoint names it; the provider prefix only
    /// addresses it.
    Model,
    /// Absent while thinking is off.
    Effort,
    Tier,
    /// The version installed under the running pi, absent while they match;
    /// else a later release, once one is found.
    Update,
    /// What the session has cost so far, absent for an unpriced model. The
    /// `cost` part is the run's alone.
    Spent,
    /// What the agent is doing, whether or not a run is on: `idle`, `working`,
    /// a retry it waits out, a run stopping. Always says something.
    State,
    /// What the user calls the session, else the first thing it was asked;
    /// absent until one of the two exists.
    Session,
    /// The checkout's directory name.
    Workspace,
    /// The checkout's branch, and what its worktree has changed against
    /// `HEAD`.
    Git,
    /// The clock of the run in flight, or of the one that last ended.
    Elapsed,
    /// What the run has sent and heard back, in tokens.
    InOut,
    /// Output tokens a second, over the time the model spent streaming them.
    Speed,
    /// The share of the run's prompt tokens that were read from cache.
    Cache,
    /// What the run cost, in dollars: the session's whole spend is `spent`.
    Cost,
    /// What the run's last request occupied against the model's window.
    Ctx,
    /// How many times the run's transcript was compacted.
    Compacted,
    /// Prompts typed while the run works and not yet sent.
    Queued,
    /// The worktree the session works in, absent in the repository's own.
    Worktree,
}

impl Field {
    /// The name a layout gives this field, as the config file spells it.
    pub fn name(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }

    /// Whether this is one of the status line's own parts, the numbers it
    /// draws while a run works, rather than something pi adds to its row.
    pub fn is_part(self) -> bool {
        matches!(
            self,
            Field::Elapsed
                | Field::InOut
                | Field::Speed
                | Field::Cache
                | Field::Cost
                | Field::Ctx
                | Field::Compacted
                | Field::Queued
                | Field::Worktree
        )
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
    Style(crate::theme::Style),
    /// A theme tone with modifiers on top: `{"tone": "code", "sgr": ["bold"]}`
    /// keeps the colour the theme gives that tone and adds the emphasis.
    Toned {
        tone: Tone,
        sgr: Vec<crate::theme::Attr>,
    },
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
            Value::String(_) => Field::deserialize(v).map(Item::Field),
            Value::Object(m) if m.contains_key("field") => Styled::deserialize(v).map(Item::Styled),
            Value::Object(m) if m.contains_key("group") => Group::deserialize(v).map(Item::Group),
            Value::Object(m) if m.contains_key("text") => Text::deserialize(v).map(Item::Text),
            Value::Object(m) if m.contains_key("script") => script_item(m).map(Item::Script),
            _ => Err(serde_json::Error::custom(
                "an item is a field's name, or an object with `field`, `text`, `group` or `script`",
            )),
        };
        item.map_err(D::Error::custom)
    }
}

// `{"script": {…}}` alone, or `{"script": "weather"}` for the named form's
// defaults. The shape is checked here so the model is told what it meant.
fn script_item(m: &serde_json::Map<String, Value>) -> Result<Script, serde_json::Error> {
    if m.len() != 1 {
        return Err(serde_json::Error::custom(
            "a script item is `{\"script\": {…}}` and nothing beside it",
        ));
    }
    let inner = m.get("script").cloned().unwrap_or(Value::Null);
    let script: Script = match inner {
        Value::String(name) => serde_json::from_value(Value::Object(
            [("name".to_string(), Value::String(name))]
                .into_iter()
                .collect(),
        ))
        .map_err(serde_json::Error::custom)?,
        other => Script::deserialize(other).map_err(serde_json::Error::custom)?,
    };
    script.validate().map_err(serde_json::Error::custom)?;
    Ok(script)
}

impl<'de> Deserialize<'de> for Look {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(d)?;
        let style = |v| crate::theme::Style::deserialize(v).map(Look::Style);
        let look = match &v {
            Value::String(_) => match Tone::deserialize(v.clone()) {
                Ok(tone) => Ok(Look::Tone(tone)),
                Err(not_tone) => style(v).map_err(|not_style| {
                    serde_json::Error::custom(format!("{not_tone}, nor a style: {not_style}"))
                }),
            },
            Value::Object(m) if m.contains_key("tone") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct Toned {
                    tone: Tone,
                    #[serde(default)]
                    sgr: Vec<crate::theme::Attr>,
                }
                match serde_json::from_value::<Toned>(v.clone()) {
                    Ok(toned) => Ok(Look::Toned {
                        tone: toned.tone,
                        sgr: toned.sgr,
                    }),
                    Err(e) => Err(serde_json::Error::custom(e)),
                }
            }
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
    pub field: Field,
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
        Item::Group(_) | Item::Field(Field::Tabs) => true,
        Item::Styled(s) => s.field == Field::Tabs,
        Item::Text(_) | Item::Field(_) | Item::Script(_) => false,
    });
    match nested {
        true => Err(D::Error::custom(
            "a group holds texts and fields, but not `tabs` or another group",
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
    fn fields_are_bare_names_and_text_carries_a_tone() {
        let layout: Layout = serde_json::from_str(
            r#"{"lines":[{"left":["tabs",{"text":"main*","style":"ok"}],"right":["flash"]},{}]}"#,
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
            r#"{"left":["ctx",{"group":[{"text":"$"},{"field":"cost","style":"ok"}]}]}"#,
        )
        .unwrap();
        assert_eq!(line.left[0], Item::Field(Field::Ctx));
        let Item::Group(g) = &line.left[1] else {
            panic!("{line:?}");
        };
        assert!(matches!(&g.group[1], Item::Styled(s) if s.field == Field::Cost));

        let tabs = parse(r#"{"lines":[{"left":[{"group":["tabs"]}]}]}"#).unwrap_err();
        assert!(tabs.contains("not `tabs`"), "{tabs}");
        let clock = parse(r#"{"lines":[{"left":["clock"]}]}"#).unwrap_err();
        assert!(clock.contains("unknown variant `clock`"), "{clock}");
        // One list: pi's own names and the status line's, read as one.
        assert!(clock.contains("`spent`, `state`"), "{clock}");
        assert!(clock.contains("`elapsed`"), "{clock}");
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
            r#"{"left":[{"field":"model","style":"36"},"tier"],"sep":{"text":"  "}}"#,
        )
        .unwrap();
        assert!(
            matches!(&line.left[0], Item::Styled(s) if s.field == Field::Model),
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
        let bad = r#"{"lines":[{"left":["tabs",{"field":"model","style":{"color":"add"}}]}]}"#;
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
