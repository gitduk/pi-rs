//! What a key press means, and where that is written down.
//!
//! The namespace is the object acted on (`edit.*`, `move.*`, `menu.*`), and,
//! following from that, it decides *when* a binding is live — so two actions
//! may share a key as long as they are never live together: `up` is
//! `menu.previous` while a list is open, `history.older` when it is not.

mod text;

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyModifiers};

pub use text::{chord, parse};

/// Which of the two modal states the editor is in. Exclusive: exactly one
/// holds at a time, which is why it is a value of its own rather than two
/// more layers — nothing here can express being in both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    // Keys type. The layer is empty: everything Insert does, it does by
    // falling through to `Editor`.
    #[default]
    Insert,
    // Keys command. Bare characters move and delete instead of typing.
    Normal,
}

/// When a binding is consulted. `action` tries these nearest-first, so a key
/// the menu claims never reaches the editor underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum When {
    // A view read, not typed into — a reply, the conversation alone. It has
    // the keyboard: the editor's layers are not consulted under it.
    Pager,
    // A list's movement and dismissal keys: over the line while completing,
    // and under the picker's own letters.
    Menu,
    // A list that has the keyboard — the rewind selector. Nothing types
    // under it, so bare letters are free to move it.
    Picker,
    // A turn is in flight.
    Run,
    // Only in that mode, and only while vim keys are on at all.
    Mode(Mode),
    // Normal with nothing on the line. Tried before `Mode`, so a key naming
    // the empty line wins over the layer that holds either way.
    NormalEmpty,
    // Always — in both modes, so bindings older than the vim layer keep
    // working under it.
    Editor,
    // Whatever has the keyboard: the stop and the way out.
    App,
}

/// Who has the keyboard, which picks the set of keys read. Only the line's
/// two stack the editor's layers; the others shut them out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Surface {
    #[default]
    Editor,
    // The line, a completion list over it: the list's keys first, and
    // letters still type, to narrow it.
    Completion,
    // The rewind selector.
    Picker,
    // A command's reply or the conversation view.
    Pager,
}

/// Which layers are up when a key is pressed. One value rather than three
/// bare fields — they are usually live at once, and a swapped pair compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Layers {
    pub surface: Surface,
    pub run: bool,
    /// The mode, or `None` when vim keys are off. Three legal states in three
    /// representations: a separate `vim: bool` beside a `Mode` would make
    /// "off, but in Normal" expressible and meaningless.
    pub mode: Option<Mode>,
    /// Whether the line has anything on it. Read only in Normal, by the keys
    /// that command the history instead when there is nothing to command.
    pub line_empty: bool,
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
    RunInterrupt,
    Rewind,
    // The conversation alone: the thinking, the calls and the editor itself
    // out of the way. `v` with nothing on the line.
    Browse,
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
    PagerDown,
    PagerUp,
    PagerHalfDown,
    PagerHalfUp,
    PagerPageDown,
    PagerPageUp,
    PagerTop,
    PagerBottom,
    PagerClose,
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
        when: W::Editor,
        keys: &["alt+enter", "ctrl+j", "shift+enter"],
        note: "",
    },
    Binding {
        id: "edit.delete.char-back",
        action: A::DeleteCharBack,
        when: W::Editor,
        keys: &["backspace", "ctrl+h"],
        note: "",
    },
    Binding {
        id: "edit.delete.char-forward",
        action: A::DeleteCharForward,
        when: W::Editor,
        keys: &["delete"],
        note: "",
    },
    Binding {
        id: "edit.delete.word-back",
        action: A::DeleteWordBack,
        when: W::Editor,
        keys: &["ctrl+w", "alt+backspace", "ctrl+backspace"],
        note: "",
    },
    Binding {
        id: "edit.delete.to-line-end",
        action: A::DeleteToLineEnd,
        when: W::Editor,
        keys: &["ctrl+k"],
        note: "",
    },
    Binding {
        id: "edit.delete.to-line-start",
        action: A::DeleteToLineStart,
        when: W::Editor,
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
        id: "picker.next",
        action: A::MenuNext,
        when: W::Picker,
        keys: &["j"],
        note: "",
    },
    Binding {
        id: "picker.previous",
        action: A::MenuPrevious,
        when: W::Picker,
        keys: &["k"],
        note: "",
    },
    Binding {
        id: "picker.accept",
        action: A::MenuAccept,
        when: W::Picker,
        keys: &["enter"],
        note: "",
    },
    Binding {
        id: "picker.close",
        action: A::MenuDismiss,
        when: W::Picker,
        keys: &["q"],
        note: "the rewind selector",
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
        keys: &["pageup"],
        note: "",
    },
    Binding {
        id: "view.scroll-down",
        action: A::ScrollPageDown,
        when: W::Editor,
        keys: &["pagedown"],
        note: "",
    },
    Binding {
        id: "view.scroll-half-up",
        action: A::ScrollHalfUp,
        when: W::Editor,
        keys: &["ctrl+b"],
        note: "",
    },
    Binding {
        id: "view.scroll-half-down",
        action: A::ScrollHalfDown,
        when: W::Editor,
        keys: &["ctrl+f"],
        note: "",
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
        id: "normal.browse",
        action: A::Browse,
        when: W::NormalEmpty,
        keys: &["v"],
        note: "the conversation alone, with nothing on the line",
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
        when: W::Editor,
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
        id: "pager.down",
        action: A::PagerDown,
        when: W::Pager,
        keys: &["j", "down"],
        note: "",
    },
    Binding {
        id: "pager.up",
        action: A::PagerUp,
        when: W::Pager,
        keys: &["k", "up"],
        note: "",
    },
    Binding {
        id: "pager.half-down",
        action: A::PagerHalfDown,
        when: W::Pager,
        keys: &["ctrl+d", "J"],
        note: "",
    },
    Binding {
        id: "pager.half-up",
        action: A::PagerHalfUp,
        when: W::Pager,
        keys: &["ctrl+u", "K"],
        note: "",
    },
    Binding {
        id: "pager.page-down",
        action: A::PagerPageDown,
        when: W::Pager,
        keys: &["ctrl+f", "pagedown"],
        note: "",
    },
    Binding {
        id: "pager.page-up",
        action: A::PagerPageUp,
        when: W::Pager,
        keys: &["ctrl+b", "pageup"],
        note: "",
    },
    Binding {
        id: "pager.top",
        action: A::PagerTop,
        when: W::Pager,
        keys: &["g g"],
        note: "",
    },
    Binding {
        id: "pager.bottom",
        action: A::PagerBottom,
        when: W::Pager,
        keys: &["G"],
        note: "",
    },
    Binding {
        id: "pager.close",
        action: A::PagerClose,
        when: W::Pager,
        keys: &["q", "esc", "enter", "v"],
        note: "a command's reply, or the conversation view",
    },
];

/// A key press, normalized. Shift folds into the character (`shift+a` is `A`);
/// ctrl/alt name the unshifted letter, where terminals disagree about the character.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Press {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Press {
    /// A character with no `ctrl` or `alt` on it: text as much as a key.
    pub fn is_bare(&self) -> bool {
        matches!(self.code, KeyCode::Char(_))
            && !self
                .mods
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
    }

    pub fn of(code: KeyCode, mods: KeyModifiers) -> Self {
        let mods = mods & (KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT);
        match code {
            KeyCode::Char(c) if mods.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
                Press {
                    code: KeyCode::Char(c.to_ascii_lowercase()),
                    mods,
                }
            }
            KeyCode::Char(c) => Press {
                code: KeyCode::Char(if mods.contains(KeyModifiers::SHIFT) {
                    c.to_ascii_uppercase()
                } else {
                    c
                }),
                mods: mods - KeyModifiers::SHIFT,
            },
            _ => Press { code, mods },
        }
    }
}

/// The character a press types, when it types one on its own: a letter with
/// `ctrl` or `alt` on it is the menu's key rather than the surface's own, which
/// is the same rule every screen with a letter vocabulary reads — browse and a
/// reply — so it is written down once, here.
pub fn bare_letter(key: &crossterm::event::KeyEvent) -> Option<char> {
    match key.code {
        KeyCode::Char(c) if Press::of(key.code, key.modifiers).is_bare() => Some(c),
        _ => None,
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
#[derive(Debug)]
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
        // Defaults fill what the user hasn't claimed; one landing on an
        // explicit key yields, one repeating another default is a table bug.
        for b in BINDINGS {
            if overrides.contains_key(b.id) {
                continue;
            }
            for spec in b.keys {
                let chord = chord(spec).map_err(|e| anyhow::anyhow!("{}: {e}", b.id))?;
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
        if press.mods.contains(KeyModifiers::SHIFT) {
            return find(Press {
                mods: press.mods - KeyModifiers::SHIFT,
                ..press
            });
        }
        None
    }
}

// The layers consulted for `layers`, innermost first.
fn live(layers: Layers) -> Vec<When> {
    let mut live = match layers.surface {
        Surface::Pager => return vec![When::Pager, When::App],
        Surface::Picker => return vec![When::Picker, When::Menu, When::App],
        // `menu.dismiss` and `run.interrupt` both claim `esc`; dismissing
        // clears the menu so the next press reaches the run.
        Surface::Completion => vec![When::Menu],
        Surface::Editor => Vec::with_capacity(6),
    };
    if layers.run {
        live.push(When::Run);
    }
    if let Some(mode) = layers.mode {
        // The line's own layer first: a key that names the empty line wins
        // over the layer that holds whether or not there is one.
        if mode == Mode::Normal && layers.line_empty {
            live.push(When::NormalEmpty);
        }
        live.push(When::Mode(mode));
    }
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

    // A reply or the conversation view reads its own keys and the app's,
    // never the editor's: one place has the keyboard.
    #[test]
    fn a_pager_has_the_keyboard() {
        let k = Keys::default();
        let pager = Layers {
            surface: Surface::Pager,
            run: true,
            mode: Some(Mode::Normal),
            line_empty: true,
        };
        assert_eq!(k.action(press("j"), pager), Some(Action::PagerDown));
        assert_eq!(
            k.action(press("ctrl+d"), pager),
            Some(Action::PagerHalfDown)
        );
        assert_eq!(k.action(press("ctrl+w"), pager), None, "the editor's");
        assert_eq!(k.action(press("ctrl+c"), pager), Some(Action::AppCancel));
        assert_eq!(
            k.hit(Some(press("g")), press("g"), pager),
            Some(Hit {
                action: Action::PagerTop,
                pair: true
            })
        );
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
                    mode: None,
                    line_empty: false,
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
            for mode in [None, Some(Mode::Insert), Some(Mode::Normal)] {
                assert_eq!(
                    k.action(
                        key,
                        Layers {
                            surface: Surface::Editor,
                            run: true,
                            mode,
                            line_empty: false,
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

    #[test]
    fn the_normal_layer_takes_nothing_away() {
        // The premise the whole table rests on: Normal binds only bare
        // characters, so every binding older than vim still answers under it.
        let k = Keys::default();
        for b in BINDINGS {
            // Both halves of Normal: about what it does to pre-vim bindings,
            // not the modal keys themselves.
            if matches!(b.when, W::Mode(_) | W::NormalEmpty | W::Pager | W::Picker) {
                continue;
            }
            for spec in b.keys {
                let (prev, press) = match chord(spec).unwrap() {
                    Chord::One(p) => (None, p),
                    Chord::Two(a, p) => (Some(a), p),
                };
                let insert = Layers {
                    surface: if b.when == W::Menu {
                        Surface::Completion
                    } else {
                        Surface::Editor
                    },
                    run: b.when == W::Run,
                    mode: None,
                    line_empty: false,
                };
                assert_eq!(
                    k.hit(prev, press, insert),
                    k.hit(
                        prev,
                        press,
                        Layers {
                            mode: Some(Mode::Normal),
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
    fn the_normal_layer_is_dead_while_vim_is_off() {
        // `mode: None` is what off means, and it is the only representation of
        // it: there is no flag beside a mode that could disagree with it.
        let k = Keys::default();
        assert_eq!(k.action(press("H"), Layers::default()), None);
        assert_eq!(
            k.action(
                press("H"),
                Layers {
                    mode: Some(Mode::Normal),
                    ..Layers::default()
                }
            ),
            Some(Action::LanePrev)
        );
    }
    #[test]
    fn normal_tells_a_capital_from_its_lowercase() {
        // What shift-folding buys: `x` and `X` delete in two directions,
        // distinct presses rather than one collapsed together.
        let k = Keys::default();
        let normal = Layers {
            mode: Some(Mode::Normal),
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
            mode: Some(Mode::Normal),
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
        for mode in [None, Some(Mode::Insert), Some(Mode::Normal)] {
            let layers = Layers {
                mode,
                ..Layers::default()
            };
            assert_eq!(k.action(press("ctrl+l"), layers), Some(Action::LineClear));
            assert_eq!(k.action(press("ctrl+c"), layers), Some(Action::AppCancel));
        }
    }

    // `v` names the state of the line, not the mode: empty shows the whole
    // conversation, otherwise it's just the character vim made it.
    #[test]
    fn browse_is_the_empty_lines_v() {
        let k = Keys::default();
        let normal = |line_empty| Layers {
            mode: Some(Mode::Normal),
            line_empty,
            ..Layers::default()
        };
        assert_eq!(k.action(press("v"), normal(true)), Some(Action::Browse));
        assert_eq!(k.action(press("v"), normal(false)), None);
        // And not with the keys off, where `v` is a letter.
        assert_eq!(
            k.action(
                press("v"),
                Layers {
                    line_empty: true,
                    ..Layers::default()
                }
            ),
            None
        );
    }

    #[test]
    fn opening_lines_and_buffer_jumps_reach_the_table() {
        let k = Keys::default();
        let normal = Layers {
            mode: Some(Mode::Normal),
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
            Press::of(KeyCode::Char('A'), KeyModifiers::NONE),
            Press::of(KeyCode::Char('A'), KeyModifiers::SHIFT),
            Press::of(KeyCode::Char('a'), KeyModifiers::SHIFT),
        ];
        assert!(reports.iter().all(|p| *p == reports[0]), "{reports:?}");
        // Ctrl+Shift rides beside Ctrl on the terminals that report it, so
        // ctrl+shift+t can own an action of its own there.
        assert_ne!(press("ctrl+shift+t"), press("ctrl+t"));
        assert_eq!(
            press("ctrl+shift+t"),
            Press::of(
                KeyCode::Char('T'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
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
        assert_eq!(
            press("D"),
            Press::of(KeyCode::Char('D'), KeyModifiers::NONE)
        );
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
