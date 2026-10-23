//! What a key press means, and where that is written down.
//!
//! The namespace is the object acted on (`edit.*`, `move.*`, `menu.*`), and,
//! following from that, it decides *when* a binding is live — so two actions
//! may share a key as long as they are never live together: `up` is
//! `menu.previous` while a list is open, `history.older` when it is not.

mod text;

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};

pub use text::{chord, parse};

/// Which of the two modal states the editor is in. Exclusive: exactly one
/// holds at a time, which is why it is a value of its own rather than two
/// more layers — nothing here can express being in both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    // Keys type, and the keys that edit as typing does are live. Vim off
    // reads keys as this, with no way into Normal.
    #[default]
    Insert,
    // Keys command. Bare characters move and delete instead of typing.
    Normal,
}

/// When a binding is consulted. `action` tries these nearest-first, so a key
/// the menu claims never reaches the editor underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum When {
    // A question's options over an empty line: they hold the keyboard
    // until one is sent or the question is closed.
    Question,
    // Something is being read — a page up over the transcript, or a search
    // on it: `esc` lets it go before it can reach the run.
    Page,
    // A list's movement and dismissal keys, over the line while completing.
    Menu,
    // A turn is in flight.
    Run,
    // Only in that mode.
    Mode(Mode),
    // Normal with nothing on the line and something read: the line has
    // nothing to move through, so its movement keys move what is read.
    NormalPage,
    // In both modes: moving, sending, scrolling — what the line is for,
    // not how its text is written.
    Editor,
    // Whatever has the keyboard: the stop and the way out.
    App,
}

/// What is over the line, which picks the set of keys read: the line's own
/// layers, under a completion list's when one is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Surface {
    #[default]
    Editor,
    // The line, a completion list over it: the list's keys first, and
    // letters still type, to narrow it.
    Completion,
    // The line, empty, with a question's options over it.
    Question,
}

/// Which layers are up when a key is pressed. One value rather than three
/// bare fields — they are usually live at once, and a swapped pair compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Layers {
    pub surface: Surface,
    pub run: bool,
    /// The mode. Vim off is Insert here: the caller never enters Normal.
    pub mode: Mode,
    /// Whether the line has anything on it. Read only in Normal, by the keys
    /// that command the history instead when there is nothing to command.
    pub line_empty: bool,
    /// Whether something is being read: a page up over the transcript, or a
    /// search on it.
    pub page: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    InsertNewline,
    DeleteCharBack,
    DeleteCharForward,
    DeleteWordBack,
    DeleteToLineEnd,
    DeleteToLineStart,
    // The line, whole: vim's `dd`.
    DeleteLine,
    MoveCharLeft,
    MoveCharRight,
    MoveWordLeft,
    MoveWordRight,
    // To the start of the next word, which `MoveWordRight` does not do: it
    // lands on the end of this one. Vim's `w` beside vim's `e`.
    MoveWordNext,
    MoveLineStart,
    MoveLineEnd,
    MoveLineFirstNonBlank,
    // The ends of the whole text, or of the history when the line is empty.
    MoveBufferStart,
    MoveBufferEnd,
    HistoryOlder,
    HistoryNewer,
    LineSubmit,
    // The line, cleared.
    LineClear,
    // A fresh session, the old one kept on disk.
    SessionNew,
    MenuAccept,
    MenuNext,
    MenuPrevious,
    MenuDismiss,
    // A question's options: moved through, one sent as if typed, or let go.
    QuestionNext,
    QuestionPrevious,
    QuestionAccept,
    // The option a digit numbers; any other key picks the highlighted one.
    QuestionPick,
    // Where several may be picked: the highlighted one in or out.
    QuestionToggle,
    // The keyboard back to the line, to answer in one's own words.
    QuestionWrite,
    QuestionDecline,
    RunInterrupt,
    Rewind,
    // The page up searched, or the transcript with none: the query typed
    // at once.
    Search,
    ScrollPageUp,
    ScrollPageDown,
    ScrollHalfUp,
    ScrollHalfDown,
    AppExit,
    // The run in flight stopped, or whatever is over the editor closed.
    AppCancel,
    // Leave, from anywhere: the cancel key twice.
    AppQuit,
    LanePrev,
    LaneNext,
    ThinkFold,
    ThinkFoldAll,
    // Into Normal from Insert; the sequence's first character comes back off
    // the line.
    ModeNormal,
    ModeInsert,
    ModeInsertAfter,
    ModeInsertLineStart,
    ModeInsertLineEnd,
    // Delete and leave in one press. Vim spells these `cl` and `c$`; there is
    // no operator here, so the two ranges worth having are bound directly.
    ChangeChar,
    ChangeToLineEnd,
    // The line, rewritten from empty: `S` the single press, `cc` the
    // doubled one.
    ChangeLine,
    // A fresh line beside the caret's, then Insert: vim's `o` and `O`.
    OpenLineBelow,
    OpenLineAbove,
    // The line, in `$EDITOR`. The one action that leaves the process.
    EditExternally,
    // A clipboard image, saved to a file whose path goes on the line.
    PasteImage,
    // A page up over the transcript, a row at a time.
    ScrollLineDown,
    ScrollLineUp,
    PageClose,
    // The next and previous match of what the page was searched for.
    PageSearchNext,
    PageSearchPrev,
}

struct Binding {
    pub id: &'static str,
    pub action: Action,
    pub when: When,
    /// Written the way a config writes them, so the parser is exercised by the
    /// defaults themselves rather than only by what a user types.
    pub keys: &'static [&'static str],
    /// Only where the id under-describes; getting naming right keeps most
    /// of these empty, so the few that say something aren't buried.
    pub note: &'static str,
}

use Action as A;
use When as W;

const BINDINGS: &[Binding] = &[
    Binding {
        id: "edit.insert.newline",
        action: A::InsertNewline,
        when: W::Mode(Mode::Insert),
        keys: &["alt+enter", "ctrl+j", "shift+enter"],
        note: "",
    },
    Binding {
        id: "edit.delete.char-back",
        action: A::DeleteCharBack,
        when: W::Mode(Mode::Insert),
        keys: &["backspace", "ctrl+h"],
        note: "",
    },
    Binding {
        id: "edit.delete.char-forward",
        action: A::DeleteCharForward,
        when: W::Mode(Mode::Insert),
        keys: &["delete"],
        note: "",
    },
    Binding {
        id: "edit.delete.word-back",
        action: A::DeleteWordBack,
        when: W::Mode(Mode::Insert),
        keys: &["ctrl+w", "alt+backspace", "ctrl+backspace"],
        note: "",
    },
    Binding {
        id: "edit.delete.to-line-end",
        action: A::DeleteToLineEnd,
        when: W::Mode(Mode::Insert),
        keys: &["ctrl+k"],
        note: "",
    },
    Binding {
        id: "edit.delete.to-line-start",
        action: A::DeleteToLineStart,
        when: W::Mode(Mode::Insert),
        keys: &["ctrl+u"],
        note: "",
    },
    Binding {
        id: "move.char.left",
        action: A::MoveCharLeft,
        when: W::Editor,
        keys: &["left"],
        note: "",
    },
    Binding {
        id: "move.char.right",
        action: A::MoveCharRight,
        when: W::Editor,
        keys: &["right"],
        note: "",
    },
    Binding {
        id: "move.word.left",
        action: A::MoveWordLeft,
        when: W::Editor,
        keys: &["alt+left", "ctrl+left", "alt+b"],
        note: "",
    },
    Binding {
        id: "move.word.right",
        action: A::MoveWordRight,
        when: W::Editor,
        keys: &["alt+right", "ctrl+right", "alt+f"],
        note: "",
    },
    Binding {
        id: "move.line.start",
        action: A::MoveLineStart,
        when: W::Editor,
        keys: &["home", "ctrl+a"],
        note: "",
    },
    Binding {
        id: "move.line.end",
        action: A::MoveLineEnd,
        when: W::Editor,
        keys: &["end", "ctrl+e"],
        note: "",
    },
    Binding {
        id: "history.older",
        action: A::HistoryOlder,
        when: W::Editor,
        keys: &["up"],
        note: "or up a line, within a multi-line prompt",
    },
    Binding {
        id: "history.newer",
        action: A::HistoryNewer,
        when: W::Editor,
        keys: &["down"],
        note: "or down a line, within a multi-line prompt",
    },
    Binding {
        id: "line.submit",
        action: A::LineSubmit,
        when: W::Editor,
        keys: &["enter"],
        note: "reaches the run in flight; a command waits for it",
    },
    Binding {
        id: "line.clear",
        action: A::LineClear,
        when: W::Editor,
        keys: &["ctrl+l"],
        note: "",
    },
    Binding {
        id: "session.new",
        action: A::SessionNew,
        when: W::Editor,
        keys: &["ctrl+l ctrl+l"],
        note: "with nothing on the line; this one kept on disk",
    },
    Binding {
        id: "menu.accept",
        action: A::MenuAccept,
        when: W::Menu,
        keys: &["tab"],
        note: "",
    },
    Binding {
        id: "menu.next",
        action: A::MenuNext,
        when: W::Menu,
        keys: &["down", "ctrl+n", "ctrl+j"],
        note: "",
    },
    Binding {
        id: "menu.previous",
        action: A::MenuPrevious,
        when: W::Menu,
        keys: &["up", "ctrl+p", "ctrl+k"],
        note: "",
    },
    Binding {
        id: "menu.dismiss",
        action: A::MenuDismiss,
        when: W::Menu,
        keys: &["esc"],
        note: "until the next keystroke",
    },
    Binding {
        id: "question.next",
        action: A::QuestionNext,
        when: W::Question,
        keys: &["j", "down", "ctrl+n"],
        note: "",
    },
    Binding {
        id: "question.previous",
        action: A::QuestionPrevious,
        when: W::Question,
        keys: &["k", "up", "ctrl+p"],
        note: "",
    },
    Binding {
        id: "question.accept",
        action: A::QuestionAccept,
        when: W::Question,
        keys: &["enter"],
        note: "sends the highlighted option as if typed",
    },
    Binding {
        id: "question.pick",
        action: A::QuestionPick,
        when: W::Question,
        keys: &["1", "2", "3", "4", "5", "6", "7", "8", "9"],
        note: "the option of that number",
    },
    Binding {
        id: "question.toggle",
        action: A::QuestionToggle,
        when: W::Question,
        keys: &["space"],
        note: "where several may be picked; a digit then toggles too",
    },
    Binding {
        id: "question.write",
        action: A::QuestionWrite,
        when: W::Question,
        keys: &["i"],
        note: "type your own answer; sending it closes the question, emptying the line returns",
    },
    Binding {
        id: "question.decline",
        action: A::QuestionDecline,
        when: W::Question,
        keys: &["q", "esc"],
        note: "closes it unanswered; any other key does nothing while it is up",
    },
    Binding {
        id: "run.interrupt",
        action: A::RunInterrupt,
        when: W::Run,
        keys: &["esc"],
        note: "before the model answers, it takes the prompt back to the editor",
    },
    Binding {
        id: "conversation.rewind",
        action: A::Rewind,
        when: W::Editor,
        keys: &["esc esc"],
        note: "with an empty line, to go back to something you said",
    },
    Binding {
        id: "view.scroll-up",
        action: A::ScrollPageUp,
        when: W::Editor,
        keys: &["pageup", "ctrl+b"],
        note: "",
    },
    Binding {
        id: "view.scroll-down",
        action: A::ScrollPageDown,
        when: W::Editor,
        keys: &["pagedown", "ctrl+f"],
        note: "",
    },
    Binding {
        id: "view.search",
        action: A::Search,
        when: W::Editor,
        keys: &["ctrl+s"],
        note: "the page up, or the transcript",
    },
    Binding {
        id: "app.exit",
        action: A::AppExit,
        when: W::Editor,
        keys: &["ctrl+d"],
        note: "only when the line is empty",
    },
    Binding {
        id: "app.cancel",
        action: A::AppCancel,
        when: W::App,
        keys: &["ctrl+c"],
        note: "stops the run in flight, closing whatever is over the editor",
    },
    Binding {
        id: "app.quit",
        action: A::AppQuit,
        when: W::App,
        keys: &["ctrl+c ctrl+c"],
        note: "",
    },
    Binding {
        id: "think.fold",
        action: A::ThinkFold,
        when: W::Editor,
        keys: &["ctrl+t"],
        note: "the last group of calls and reasoning in full, or on one line",
    },
    Binding {
        id: "think.fold-all",
        action: A::ThinkFoldAll,
        when: W::Editor,
        keys: &["ctrl+shift+t", "alt+t"],
        note: "every group of calls and reasoning, the last one included",
    },
    // Normal mode from here down: every key is a bare character, since this
    // layer only adds atop `Editor`. `normal.` prefixes an id only where needed.
    Binding {
        id: "normal.lane.prev",
        action: A::LanePrev,
        when: W::Mode(Mode::Normal),
        keys: &["H"],
        note: "the previous checkout, opening it if it is not",
    },
    Binding {
        id: "normal.lane.next",
        action: A::LaneNext,
        when: W::Mode(Mode::Normal),
        keys: &["L"],
        note: "the next checkout, opening it if it is not",
    },
    // The window, where the lowercase pair moves through recall instead.
    Binding {
        id: "normal.view.scroll-half-up",
        action: A::ScrollHalfUp,
        when: W::Mode(Mode::Normal),
        keys: &["K"],
        note: "half a window back, as ctrl+b does",
    },
    Binding {
        id: "normal.view.scroll-half-down",
        action: A::ScrollHalfDown,
        when: W::Mode(Mode::Normal),
        keys: &["J"],
        note: "half a window on, as ctrl+f does",
    },
    Binding {
        id: "normal.search",
        action: A::Search,
        when: W::Mode(Mode::Normal),
        keys: &["/"],
        note: "the page up, or the transcript",
    },
    Binding {
        id: "normal.move.char.left",
        action: A::MoveCharLeft,
        when: W::Mode(Mode::Normal),
        keys: &["h"],
        note: "",
    },
    Binding {
        id: "normal.move.char.right",
        action: A::MoveCharRight,
        when: W::Mode(Mode::Normal),
        keys: &["l"],
        note: "",
    },
    Binding {
        id: "normal.move.word.next",
        action: A::MoveWordNext,
        when: W::Mode(Mode::Normal),
        keys: &["w"],
        note: "the start of the next word",
    },
    Binding {
        id: "normal.move.word.end",
        action: A::MoveWordRight,
        when: W::Mode(Mode::Normal),
        keys: &["e"],
        note: "the end of this one",
    },
    Binding {
        id: "normal.move.word.back",
        action: A::MoveWordLeft,
        when: W::Mode(Mode::Normal),
        keys: &["b"],
        note: "",
    },
    Binding {
        id: "normal.move.line.start",
        action: A::MoveLineStart,
        when: W::Mode(Mode::Normal),
        keys: &["0"],
        note: "",
    },
    Binding {
        id: "normal.move.line.end",
        action: A::MoveLineEnd,
        when: W::Mode(Mode::Normal),
        keys: &["$"],
        note: "",
    },
    Binding {
        id: "normal.move.line.first-non-blank",
        action: A::MoveLineFirstNonBlank,
        when: W::Mode(Mode::Normal),
        keys: &["^"],
        note: "",
    },
    Binding {
        id: "normal.move.buffer.end",
        action: A::MoveBufferEnd,
        when: W::Mode(Mode::Normal),
        keys: &["G"],
        note: "the history's end when the line is empty",
    },
    Binding {
        id: "normal.move.buffer.start",
        action: A::MoveBufferStart,
        when: W::Mode(Mode::Normal),
        keys: &["g g"],
        note: "the history's start when the line is empty",
    },
    Binding {
        id: "normal.delete.line",
        action: A::DeleteLine,
        when: W::Mode(Mode::Normal),
        keys: &["d d"],
        note: "",
    },
    Binding {
        id: "normal.history.older",
        action: A::HistoryOlder,
        when: W::Mode(Mode::Normal),
        keys: &["k"],
        note: "the line above, or the previous prompt when there is none",
    },
    Binding {
        id: "normal.history.newer",
        action: A::HistoryNewer,
        when: W::Mode(Mode::Normal),
        keys: &["j"],
        note: "",
    },
    Binding {
        id: "normal.delete.char-forward",
        action: A::DeleteCharForward,
        when: W::Mode(Mode::Normal),
        keys: &["x"],
        note: "",
    },
    Binding {
        id: "normal.delete.char-back",
        action: A::DeleteCharBack,
        when: W::Mode(Mode::Normal),
        keys: &["X"],
        note: "",
    },
    Binding {
        id: "normal.delete.to-line-end",
        action: A::DeleteToLineEnd,
        when: W::Mode(Mode::Normal),
        keys: &["D"],
        note: "",
    },
    Binding {
        id: "normal.change.char",
        action: A::ChangeChar,
        when: W::Mode(Mode::Normal),
        keys: &["s"],
        note: "the character under the caret, then Insert",
    },
    Binding {
        id: "normal.change.to-line-end",
        action: A::ChangeToLineEnd,
        when: W::Mode(Mode::Normal),
        keys: &["C"],
        note: "to the end of the line, then Insert",
    },
    Binding {
        id: "normal.change.line",
        action: A::ChangeLine,
        when: W::Mode(Mode::Normal),
        keys: &["S", "c c"],
        note: "the whole line, then Insert",
    },
    Binding {
        id: "normal.open.below",
        action: A::OpenLineBelow,
        when: W::Mode(Mode::Normal),
        keys: &["o"],
        note: "a new line under the caret's, then Insert",
    },
    Binding {
        id: "normal.open.above",
        action: A::OpenLineAbove,
        when: W::Mode(Mode::Normal),
        keys: &["O"],
        note: "a new line over it, then Insert",
    },
    Binding {
        id: "edit.paste.image",
        action: A::PasteImage,
        when: W::Mode(Mode::Insert),
        keys: &["ctrl+v"],
        note: "the clipboard's image, saved under ~/.pi/images",
    },
    Binding {
        id: "normal.edit.external",
        action: A::EditExternally,
        when: W::Mode(Mode::Normal),
        keys: &["E"],
        note: "the line in $VISUAL/$EDITOR, and back",
    },
    Binding {
        id: "mode.normal",
        action: A::ModeNormal,
        when: W::Mode(Mode::Insert),
        keys: &["j k"],
        note: "typed quickly; the j comes back off the line",
    },
    Binding {
        id: "mode.insert",
        action: A::ModeInsert,
        when: W::Mode(Mode::Normal),
        keys: &["i"],
        note: "",
    },
    Binding {
        id: "mode.insert.after",
        action: A::ModeInsertAfter,
        when: W::Mode(Mode::Normal),
        keys: &["a"],
        note: "past the caret",
    },
    Binding {
        id: "mode.insert.line-start",
        action: A::ModeInsertLineStart,
        when: W::Mode(Mode::Normal),
        keys: &["I"],
        note: "",
    },
    Binding {
        id: "mode.insert.line-end",
        action: A::ModeInsertLineEnd,
        when: W::Mode(Mode::Normal),
        keys: &["A"],
        note: "",
    },
    Binding {
        id: "page.close",
        action: A::PageClose,
        when: W::Page,
        keys: &["esc"],
        note: "a command's output, a preview, or a search on the transcript",
    },
    Binding {
        id: "page.down",
        action: A::ScrollLineDown,
        when: W::NormalPage,
        keys: &["j"],
        note: "",
    },
    Binding {
        id: "page.up",
        action: A::ScrollLineUp,
        when: W::NormalPage,
        keys: &["k"],
        note: "",
    },
    Binding {
        id: "page.quit",
        action: A::PageClose,
        when: W::NormalPage,
        keys: &["q"],
        note: "",
    },
    Binding {
        id: "page.search-next",
        action: A::PageSearchNext,
        when: W::NormalPage,
        keys: &["n"],
        note: "",
    },
    Binding {
        id: "page.search-prev",
        action: A::PageSearchPrev,
        when: W::NormalPage,
        keys: &["N"],
        note: "",
    },
];

/// A key a binding can name. `Other` is any key a surface reports that no
/// name reaches: a press all the same, bound to nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    F(u8),
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Other,
}

/// The modifiers a binding can name; any other a surface reports is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Self = Self {
        ctrl: false,
        alt: false,
        shift: false,
    };
}

/// A key press, normalized. Shift folds into the character (`shift+a` is `A`);
/// ctrl/alt name the unshifted letter, where terminals disagree about the character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Press {
    pub code: Key,
    pub mods: Mods,
}

impl Press {
    /// A character with no `ctrl` or `alt` on it: text as much as a key.
    pub fn is_bare(&self) -> bool {
        matches!(self.code, Key::Char(_)) && !self.mods.ctrl && !self.mods.alt
    }

    pub fn of(code: Key, mods: Mods) -> Self {
        match code {
            Key::Char(c) if mods.ctrl || mods.alt => Press {
                code: Key::Char(c.to_ascii_lowercase()),
                mods,
            },
            Key::Char(c) => Press {
                code: Key::Char(if mods.shift {
                    c.to_ascii_uppercase()
                } else {
                    c
                }),
                mods: Mods {
                    shift: false,
                    ..mods
                },
            },
            _ => Press { code, mods },
        }
    }
}

/// What a binding is written as: one press, or two in quick succession —
/// `g g`, `ctrl+c ctrl+c`. The second press of a pair is read against the
/// first only when the first is still fresh; the caller decides what fresh is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Chord {
    One(Press),
    Two(Press, Press),
}

/// What a press resolved to, and whether it finished a pair rather than
/// standing alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hit {
    pub action: Action,
    pub pair: bool,
}

/// Every binding in force, resolved once at startup.
#[derive(Debug, PartialEq)]
pub struct Keys {
    map: HashMap<(When, Chord), Action>,
    // Which binding owns each chord, so `listing` can ask "what's bound to
    // this id" instead of reconstructing it from the action.
    who: HashMap<(When, Chord), &'static str>,
}

impl Default for Keys {
    fn default() -> Self {
        Self::resolve(&BTreeMap::new()).expect("the built-in table is well formed")
    }
}

impl Keys {
    /// Defaults, with `overrides` replacing (not adding to) the key list of
    /// any id it names. An explicit binding wins over a default on the same key.
    pub fn resolve(overrides: &BTreeMap<String, Vec<String>>) -> Result<Self> {
        for id in overrides.keys() {
            if !BINDINGS.iter().any(|b| b.id == id) {
                let known: Vec<&str> = BINDINGS.iter().map(|b| b.id).collect();
                bail!("unknown key action `{id}`; known: {}", known.join(", "));
            }
        }
        let mut map: HashMap<(When, Chord), Action> = HashMap::new();
        let mut who: HashMap<(When, Chord), &str> = HashMap::new();
        // Explicit bindings first: they are authoritative over defaults, and
        // two of them on one key in one context is a genuine conflict.
        for b in BINDINGS {
            if let Some(v) = overrides.get(b.id) {
                for spec in v {
                    let chord = chord(spec).map_err(|e| anyhow::anyhow!("{}: {e}", b.id))?;
                    if let Some(other) = who.insert((b.when, chord), b.id) {
                        bail!("`{spec}` is bound to both {other} and {} at once", b.id);
                    }
                    map.insert((b.when, chord), b.action);
                }
            }
        }
        // App keys are meant wherever the keyboard is; a nearer layer's default
        // on one would leave it dead on the line, so that default yields.
        let app_claimed: Vec<Chord> = (who.keys())
            .filter(|(when, _)| *when == When::App)
            .map(|(_, chord)| *chord)
            .collect();
        // Defaults fill what the user hasn't claimed; one landing on an
        // explicit key yields, one repeating another default is a table bug.
        for b in BINDINGS {
            if overrides.contains_key(b.id) {
                continue;
            }
            for spec in b.keys {
                let chord = chord(spec).map_err(|e| anyhow::anyhow!("{}: {e}", b.id))?;
                if b.when != When::App && app_claimed.contains(&chord) {
                    continue;
                }
                if let Some(other) = who.get(&(b.when, chord)) {
                    if overrides.contains_key(*other) {
                        continue;
                    }
                    bail!("`{spec}` is bound to both {other} and {} at once", b.id);
                }
                who.insert((b.when, chord), b.id);
                map.insert((b.when, chord), b.action);
            }
        }
        Ok(Self { map, who })
    }

    /// What this press means on its own, given what is on screen.
    pub fn action(&self, press: Press, layers: Layers) -> Option<Action> {
        self.hit(None, press, layers).map(|h| h.action)
    }

    /// What this press means, `prev` being the press before it when that one
    /// is fresh enough to pair with. Layer by layer, innermost first, a pair
    /// is tried before the press alone: within a layer the pair is the more
    /// specific, and a nearer layer's single key still outranks a farther pair.
    pub fn hit(&self, prev: Option<Press>, press: Press, layers: Layers) -> Option<Hit> {
        let live = live(layers);
        let find = |press: Press| {
            live.iter().find_map(|w| {
                if let Some(prev) = prev
                    && let Some(&action) = self.map.get(&(*w, Chord::Two(prev, press)))
                {
                    return Some(Hit { action, pair: true });
                }
                let action = *self.map.get(&(*w, Chord::One(press)))?;
                Some(Hit {
                    action,
                    pair: false,
                })
            })
        };
        if let hit @ Some(_) = find(press) {
            return hit;
        }
        // Terminals that don't report shift send `ctrl+shift+w` as `ctrl+w`;
        // falling back to the bare press keeps it working either way.
        if press.mods.shift {
            return find(Press {
                mods: Mods {
                    shift: false,
                    ..press.mods
                },
                ..press
            });
        }
        None
    }
}

// The layers consulted for `layers`, innermost first.
fn live(layers: Layers) -> Vec<When> {
    let mut live = match layers.surface {
        Surface::Completion => vec![When::Menu],
        Surface::Question => vec![When::Question],
        Surface::Editor => Vec::with_capacity(8),
    };
    // `menu.dismiss`, `page.close` and `run.interrupt` all claim `esc`:
    // each press takes away the nearest, the next reaches the one under.
    if layers.page {
        live.push(When::Page);
    }
    if layers.run {
        live.push(When::Run);
    }
    // The line's own layer first: a key that names the empty line wins
    // over the layer that holds whether or not there is one.
    if layers.mode == Mode::Normal && layers.line_empty && layers.page {
        live.push(When::NormalPage);
    }
    live.push(When::Mode(layers.mode));
    live.push(When::Editor);
    live.push(When::App);
    live
}

#[cfg(test)]
mod tests {

    #[test]
    fn folding_the_reasoning_is_reachable_while_a_run_is_in_flight() {
        // Only worth having while reasoning arrives; a binding that resolved
        // between turns, not during them, would be useless in the one context.
        let keys = Keys::resolve(&BTreeMap::new()).unwrap();
        for (surface, running) in [
            (Surface::Editor, false),
            (Surface::Editor, true),
            (Surface::Completion, true),
        ] {
            assert_eq!(
                keys.action(
                    press("ctrl+t"),
                    Layers {
                        surface,
                        run: running,
                        ..Layers::default()
                    }
                ),
                Some(Action::ThinkFold),
                "surface={surface:?} running={running}"
            );
        }
    }
    use super::*;

    pub(super) fn press(s: &str) -> Press {
        parse(s).unwrap()
    }

    const SHIFT: Mods = Mods {
        shift: true,
        ..Mods::NONE
    };

    // A page takes no keys from the line: `esc` closes it, and in Normal
    // with nothing typed the line's movement keys move it.
    #[test]
    fn a_page_borrows_only_what_the_empty_line_does_not_need() {
        let k = Keys::default();
        let page = Layers {
            surface: Surface::Editor,
            run: true,
            mode: Mode::Normal,
            line_empty: true,
            page: true,
        };
        assert_eq!(k.action(press("esc"), page), Some(Action::PageClose));
        assert_eq!(k.action(press("j"), page), Some(Action::ScrollLineDown));
        assert_eq!(k.action(press("J"), page), Some(Action::ScrollHalfDown));
        let typed = Layers {
            line_empty: false,
            ..page
        };
        assert_eq!(k.action(press("j"), typed), Some(Action::HistoryNewer));
        let insert = Layers {
            mode: Mode::Insert,
            ..page
        };
        assert_eq!(k.action(press("j"), insert), None, "typed, not bound");
        assert_eq!(k.action(press("esc"), insert), Some(Action::PageClose));
    }

    // `esc esc` is a rewind only where nothing nearer claims `esc`: with a
    // run in flight the second press stops it, as the first did.
    #[test]
    fn a_nearer_single_key_outranks_a_farther_pair() {
        let k = Keys::default();
        let running = Layers {
            run: true,
            ..Layers::default()
        };
        assert_eq!(
            k.hit(Some(press("esc")), press("esc"), running)
                .map(|h| h.action),
            Some(Action::RunInterrupt)
        );
        // Within one layer the pair is the more specific.
        assert_eq!(
            k.hit(Some(press("ctrl+c")), press("ctrl+c"), running),
            Some(Hit {
                action: Action::AppQuit,
                pair: true
            })
        );
    }

    #[test]
    fn a_binding_is_one_press_or_a_pair() {
        assert!(matches!(chord("g g"), Ok(Chord::Two(..))));
        assert!(matches!(chord("ctrl+c"), Ok(Chord::One(_))));
        assert!(chord("g g g").is_err());
        assert!(chord("  ").is_err());
    }

    #[test]
    fn the_built_in_table_resolves() {
        // Defaults are written as specs so this exercises the parser too: a
        // spelling the parser cannot read cannot reach the table unnoticed.
        let keys = Keys::resolve(&BTreeMap::new()).unwrap();
        assert_eq!(
            keys.action(press("ctrl+w"), Layers::default()),
            Some(Action::DeleteWordBack)
        );
    }

    #[test]
    fn a_key_can_mean_two_things_in_two_contexts() {
        // The reason the namespace decides scope: this is not a conflict, and a
        // flat table has no way to say so.
        let k = Keys::default();
        assert_eq!(
            k.action(
                press("up"),
                Layers {
                    surface: Surface::Completion,
                    ..Layers::default()
                }
            ),
            Some(Action::MenuPrevious)
        );
        assert_eq!(
            k.action(press("up"), Layers::default()),
            Some(Action::HistoryOlder)
        );
        assert_eq!(
            k.action(
                press("esc"),
                Layers {
                    surface: Surface::Completion,
                    run: true,
                    mode: Mode::Insert,
                    line_empty: false,
                    page: false,
                }
            ),
            Some(Action::MenuDismiss)
        );
        assert_eq!(
            k.hit(Some(press("esc")), press("esc"), Layers::default()),
            Some(Hit {
                action: Action::Rewind,
                pair: true
            })
        );
    }

    // `menu.dismiss` and `run.interrupt` share one key. Read from the table,
    // not hardcoded, so rebinding either can't make this pass by never firing.
    #[test]
    fn nothing_over_the_editor_leaves_the_stop_key_to_the_run() {
        let k = Keys::default();
        let stop = BINDINGS
            .iter()
            .find(|b| b.id == "run.interrupt")
            .expect("a binding that stops a run");
        for spec in stop.keys {
            let key = parse(spec).unwrap();
            for mode in [Mode::Insert, Mode::Normal] {
                assert_eq!(
                    k.action(
                        key,
                        Layers {
                            surface: Surface::Editor,
                            run: true,
                            mode,
                            line_empty: false,
                            page: false,
                        }
                    ),
                    Some(Action::RunInterrupt),
                    "`{spec}` with mode={mode:?}"
                );
            }
        }
    }

    #[test]
    fn an_explicit_binding_wins_over_a_default_on_the_same_key() {
        // move.line.start defaults to ctrl+a; binding move.line.end to it
        // takes the key over rather than failing startup.
        let mut o = BTreeMap::new();
        o.insert("move.line.end".to_string(), vec!["ctrl+a".to_string()]);
        let k = Keys::resolve(&o).unwrap();
        assert_eq!(
            k.action(press("ctrl+a"), Layers::default()),
            Some(Action::MoveLineEnd)
        );
        assert_eq!(
            k.action(press("home"), Layers::default()),
            Some(Action::MoveLineStart)
        );
    }

    // An app key the editor's default sat on would be accepted and never
    // fire on the line; the default yields, and keeps its other keys.
    #[test]
    fn an_explicit_app_key_is_not_shadowed_by_a_nearer_default() {
        let mut o = BTreeMap::new();
        o.insert("app.quit".to_string(), vec!["ctrl+j".to_string()]);
        let k = Keys::resolve(&o).unwrap();
        assert_eq!(
            k.action(press("ctrl+j"), Layers::default()),
            Some(Action::AppQuit)
        );
        assert_eq!(
            k.action(press("alt+enter"), Layers::default()),
            Some(Action::InsertNewline)
        );
    }

    // The completion list is filtered by letters: nothing is bound to a bare
    // letter under `Menu`, or the list could no longer be narrowed by typing.
    #[test]
    fn the_menu_leaves_the_letters_to_the_list() {
        let k = Keys::default();
        let listing = Layers {
            surface: Surface::Completion,
            ..Layers::default()
        };
        for letter in ["x", "e", "j", "k"] {
            assert_eq!(
                k.action(press(letter), listing),
                None,
                "`{letter}` was taken from the completion list"
            );
        }
    }

    // Each press answers only while a question is up; without one, the same
    // keys type, send and stop as ever.
    #[test]
    fn a_question_takes_its_keys_and_lends_them_back() {
        let k = Keys::default();
        let asked = Layers {
            surface: Surface::Question,
            run: true,
            ..Layers::default()
        };
        for (key, action) in [
            ("down", Action::QuestionNext),
            ("up", Action::QuestionPrevious),
            ("enter", Action::QuestionAccept),
            ("3", Action::QuestionPick),
            ("space", Action::QuestionToggle),
            ("esc", Action::QuestionDecline),
        ] {
            assert_eq!(k.action(press(key), asked), Some(action), "`{key}`");
        }
        assert_eq!(k.action(press("j"), asked), Some(Action::QuestionNext));
        assert_eq!(k.action(press("q"), asked), Some(Action::QuestionDecline));
        assert_eq!(k.action(press("i"), asked), Some(Action::QuestionWrite));
        assert_eq!(
            k.action(press("y"), asked),
            None,
            "a letter not bound is nothing"
        );
        let free = Layers {
            surface: Surface::Editor,
            ..asked
        };
        assert_eq!(k.action(press("3"), free), None);
        assert_eq!(k.action(press("esc"), free), Some(Action::RunInterrupt));
        assert_eq!(k.action(press("ctrl+t"), asked), Some(Action::ThinkFold));
    }

    #[test]
    fn the_normal_layer_takes_nothing_away() {
        // The premise the whole table rests on: Normal binds only bare
        // characters, so every binding outside the two modes answers in both.
        let k = Keys::default();
        for b in BINDINGS {
            // Both halves of Normal: about what it does to pre-vim bindings,
            // not the modal keys themselves.
            if matches!(b.when, W::Mode(_) | W::NormalPage | W::Page) {
                continue;
            }
            for spec in b.keys {
                let (prev, press) = match chord(spec).unwrap() {
                    Chord::One(p) => (None, p),
                    Chord::Two(a, p) => (Some(a), p),
                };
                let insert = Layers {
                    surface: match b.when {
                        W::Menu => Surface::Completion,
                        W::Question => Surface::Question,
                        _ => Surface::Editor,
                    },
                    run: b.when == W::Run,
                    mode: Mode::Insert,
                    line_empty: false,
                    page: false,
                };
                assert_eq!(
                    k.hit(prev, press, insert),
                    k.hit(
                        prev,
                        press,
                        Layers {
                            mode: Mode::Normal,
                            ..insert
                        }
                    ),
                    "{} (`{spec}`) does not mean the same thing in Normal",
                    b.id
                );
            }
        }
    }

    #[test]
    fn normal_tells_a_capital_from_its_lowercase() {
        // What shift-folding buys: `x` and `X` delete in two directions,
        // distinct presses rather than one collapsed together.
        let k = Keys::default();
        let normal = Layers {
            mode: Mode::Normal,
            ..Layers::default()
        };
        assert_eq!(
            k.action(press("x"), normal),
            Some(Action::DeleteCharForward)
        );
        assert_eq!(k.action(press("X"), normal), Some(Action::DeleteCharBack));
        assert_eq!(k.action(press("i"), normal), Some(Action::ModeInsert));
        assert_eq!(
            k.action(press("I"), normal),
            Some(Action::ModeInsertLineStart)
        );
    }

    #[test]
    fn h_and_l_move_the_caret_in_normal_mode() {
        // Unbound in Normal is a dead key — it commands nothing and types
        // nothing — so the pair has to be in the table, not merely unclaimed.
        let k = Keys::default();
        let normal = Layers {
            mode: Mode::Normal,
            ..Layers::default()
        };
        assert_eq!(k.action(press("h"), normal), Some(Action::MoveCharLeft));
        assert_eq!(k.action(press("l"), normal), Some(Action::MoveCharRight));
    }

    // The two control keys are live in both modes, so binding them is
    // `Editor`'s: one loses the line, the other the run.
    #[test]
    fn clearing_the_line_and_cancelling_are_two_keys() {
        let k = Keys::default();
        for mode in [Mode::Insert, Mode::Normal] {
            let layers = Layers {
                mode,
                ..Layers::default()
            };
            assert_eq!(k.action(press("ctrl+l"), layers), Some(Action::LineClear));
            assert_eq!(k.action(press("ctrl+c"), layers), Some(Action::AppCancel));
        }
    }

    #[test]
    fn opening_lines_and_buffer_jumps_reach_the_table() {
        let k = Keys::default();
        let normal = Layers {
            mode: Mode::Normal,
            ..Layers::default()
        };
        assert_eq!(k.action(press("o"), normal), Some(Action::OpenLineBelow));
        assert_eq!(k.action(press("O"), normal), Some(Action::OpenLineAbove));
        assert_eq!(k.action(press("S"), normal), Some(Action::ChangeLine));
        assert_eq!(k.action(press("G"), normal), Some(Action::MoveBufferEnd));
        assert_eq!(
            k.action(press("^"), normal),
            Some(Action::MoveLineFirstNonBlank)
        );
    }

    #[test]
    fn two_explicit_bindings_may_not_share_a_key() {
        let mut o = BTreeMap::new();
        o.insert("move.line.start".to_string(), vec!["ctrl+z".to_string()]);
        o.insert("move.line.end".to_string(), vec!["ctrl+z".to_string()]);
        let e = Keys::resolve(&o).unwrap_err().to_string();
        assert!(e.contains("bound to both"), "{e}");
    }

    #[test]
    fn a_config_copied_from_the_old_scroll_defaults_still_resolves() {
        // A user's explicit ctrl+b on view.scroll-up collides with the
        // half-up default; the explicit binding must win, not error.
        let mut o = BTreeMap::new();
        o.insert(
            "view.scroll-up".to_string(),
            vec!["pageup".to_string(), "ctrl+b".to_string()],
        );
        let k = Keys::resolve(&o).unwrap();
        assert_eq!(
            k.action(press("ctrl+b"), Layers::default()),
            Some(Action::ScrollPageUp)
        );
        assert_eq!(
            k.action(press("pageup"), Layers::default()),
            Some(Action::ScrollPageUp)
        );
    }

    #[test]
    fn an_override_replaces_rather_than_adds() {
        // Otherwise a default can never be removed, only buried.
        let mut o = BTreeMap::new();
        o.insert("move.line.start".to_string(), vec!["f1".to_string()]);
        let k = Keys::resolve(&o).unwrap();
        assert_eq!(
            k.action(press("f1"), Layers::default()),
            Some(Action::MoveLineStart)
        );
        assert_eq!(k.action(press("home"), Layers::default()), None);
    }

    #[test]
    fn a_misspelled_action_is_named_with_the_real_ones() {
        let mut o = BTreeMap::new();
        o.insert("move.line.begin".to_string(), vec!["home".to_string()]);
        let e = Keys::resolve(&o).unwrap_err().to_string();
        assert!(e.contains("move.line.begin"), "{e}");
        assert!(e.contains("move.line.start"), "{e}");
    }

    #[test]
    fn shift_rides_the_character_rather_than_the_modifier() {
        // Terminals disagree whether shift+a arrives as Char('A'), Char('A')
        // +SHIFT, or Char('a')+SHIFT; all three must converge to one press.
        let reports = [
            Press::of(Key::Char('A'), Mods::NONE),
            Press::of(Key::Char('A'), SHIFT),
            Press::of(Key::Char('a'), SHIFT),
        ];
        assert!(reports.iter().all(|p| *p == reports[0]), "{reports:?}");
        // Ctrl+Shift rides beside Ctrl on the terminals that report it, so
        // ctrl+shift+t can own an action of its own there.
        assert_ne!(press("ctrl+shift+t"), press("ctrl+t"));
        assert_eq!(
            press("ctrl+shift+t"),
            Press::of(
                Key::Char('T'),
                Mods {
                    ctrl: true,
                    shift: true,
                    ..Mods::NONE
                }
            )
        );
        // Named keys keep it, so shift+enter stays expressible.
        assert_ne!(press("shift+enter"), press("enter"));
    }

    #[test]
    fn a_capital_is_a_different_press_from_its_lowercase() {
        // Shift folds into the character rather than being dropped, which is
        // what lets a modal keymap give `D` a meaning `d` does not have.
        assert_ne!(press("D"), press("d"));
        assert_eq!(press("D"), Press::of(Key::Char('D'), Mods::NONE));
        // Ctrl and Alt still name the unshifted letter: there the terminals
        // disagree about the character, not about the modifier.
        assert_eq!(press("ctrl+D"), press("ctrl+d"));
        // Modifier and key names stay case-insensitive; only bare characters
        // carry case.
        assert_eq!(press("Ctrl+Left"), press("ctrl+left"));
    }

    #[test]
    fn a_capital_and_its_lowercase_can_hold_two_bindings_at_once() {
        // The payoff: `D` and `d` are now distinct presses, so a table
        // can bind both instead of giving one up.
        let mut o = BTreeMap::new();
        o.insert("move.line.start".to_string(), vec!["D".to_string()]);
        o.insert("move.line.end".to_string(), vec!["d".to_string()]);
        let k = Keys::resolve(&o).unwrap();
        assert_eq!(
            k.action(press("D"), Layers::default()),
            Some(Action::MoveLineStart)
        );
        assert_eq!(
            k.action(press("d"), Layers::default()),
            Some(Action::MoveLineEnd)
        );
    }

    #[test]
    fn a_shift_riding_press_falls_back_to_the_bare_key() {
        // `ctrl+shift+w` is its own press only where terminals report shift;
        // elsewhere it's `ctrl+w`. The fallback makes it work everywhere.
        let keys = Keys::default();
        assert_eq!(
            keys.action(press("ctrl+shift+w"), Layers::default()),
            Some(Action::DeleteWordBack)
        );
        // A binding that owns the shift press wins over the fallback.
        assert_eq!(
            keys.action(press("ctrl+shift+t"), Layers::default()),
            Some(Action::ThinkFoldAll)
        );
    }

    #[test]
    fn the_global_fold_is_reachable_on_the_terminals_that_report_shift() {
        // ctrl+shift+t degrades to ctrl+t where terminals swallow shift, so
        // alt+t stays bound as a reachable alternate either way.
        let keys = Keys::default();
        assert_eq!(
            keys.action(press("ctrl+shift+t"), Layers::default()),
            Some(Action::ThinkFoldAll)
        );
        assert_eq!(
            keys.action(press("alt+t"), Layers::default()),
            Some(Action::ThinkFoldAll)
        );
        assert_eq!(
            keys.action(press("ctrl+t"), Layers::default()),
            Some(Action::ThinkFold)
        );
    }

    #[test]
    fn every_action_is_reachable() {
        // An action with no binding is dead code that reads as a feature.
        let k = Keys::default();
        let bound: std::collections::HashSet<_> = k.map.values().copied().collect();
        for b in BINDINGS {
            assert!(bound.contains(&b.action), "{} reaches nothing", b.id);
        }
    }
}
