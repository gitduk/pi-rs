//! What a key press means, and where that is written down.
//!
//! Two ideas, neither of them Pi's. First, the namespace is the object acted
//! on — `edit.*` changes the buffer, `move.*` only the caret, `menu.*` the
//! completion list — where Pi's `tui.` prefix says nothing, everything being
//! tui. Second, and following from it, the namespace decides *when* a binding
//! is live, so two actions may share a key as long as they are never live
//! together. `up` is `menu.previous` while the list is open and
//! `history.older` when it is not, which is not a conflict and cannot be
//! expressed as one in a flat table.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyModifiers};

/// Which of the two modal states the editor is in. Exclusive: exactly one
/// holds at a time, which is why it is a value of its own rather than two
/// more layers — nothing here can express being in both at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    /// Keys type. The layer is empty: everything Insert does, it does by
    /// falling through to `Editor`.
    #[default]
    Insert,
    /// Keys command. Bare characters move and delete instead of typing.
    Normal,
}

/// When a binding is consulted. `action` tries these nearest-first, so a key
/// the menu claims never reaches the editor underneath it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum When {
    /// The completion list is open.
    Menu,
    /// A turn is in flight.
    Run,
    /// Only in that mode, and only while vim keys are on at all.
    Mode(Mode),
    /// Always — in both modes, so the thirty bindings that were here before
    /// vim existed keep working under it.
    Editor,
}

/// Which layers are up when a key is pressed. One value rather than three
/// bare fields — they are usually live at once, and a swapped pair compiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Layers {
    pub menu: bool,
    pub run: bool,
    /// The mode, or `None` when vim keys are off. Three legal states in three
    /// representations: a separate `vim: bool` beside a `Mode` would make
    /// "off, but in Normal" expressible and meaningless.
    pub mode: Option<Mode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    InsertNewline,
    DeleteCharBack,
    DeleteCharForward,
    DeleteWordBack,
    DeleteToLineEnd,
    DeleteToLineStart,
    MoveCharLeft,
    MoveCharRight,
    MoveWordLeft,
    MoveWordRight,
    /// To the start of the next word, which `MoveWordRight` does not do: it
    /// lands on the end of this one. Vim's `w` beside vim's `e`.
    MoveWordNext,
    MoveLineStart,
    MoveLineEnd,
    HistoryOlder,
    HistoryNewer,
    LineSubmit,
    LineClear,
    MenuAccept,
    MenuNext,
    MenuPrevious,
    MenuDismiss,
    RunInterrupt,
    Rewind,
    ScrollPageUp,
    ScrollPageDown,
    ScrollHalfUp,
    ScrollHalfDown,
    AppExit,
    AppClearScreen,
    LaneNext,
    ThinkFold,
    ThinkFoldAll,
    // Leaving Normal. There is no action for entering it: `jk` is a sequence,
    // not a press, and it is read where unbound characters are typed.
    ModeInsert,
    ModeInsertAfter,
    ModeInsertLineStart,
    ModeInsertLineEnd,
    // Delete and leave in one press. Vim spells these `cl` and `c$`; there is
    // no operator here, so the two ranges worth having are bound directly.
    ChangeChar,
    ChangeToLineEnd,
    /// The line, in `$EDITOR`. The one action that leaves the process.
    EditExternally,
}

pub struct Binding {
    pub id: &'static str,
    pub action: Action,
    pub when: When,
    /// Written the way a config writes them, so the parser is exercised by the
    /// defaults themselves rather than only by what a user types.
    pub keys: &'static [&'static str],
    /// Only where the id under-describes. Getting the naming right is what
    /// makes most of these empty — `move.word.left` needs no gloss, and a
    /// column that restated every id would bury the four that say something.
    pub note: &'static str,
}

use Action as A;
use When as W;

pub const BINDINGS: &[Binding] = &[
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
        note: "queues, while a run is working",
    },
    Binding {
        id: "line.clear",
        action: A::LineClear,
        when: W::Editor,
        keys: &["ctrl+c"],
        note: "twice quickly to quit, and it stops a run",
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
        keys: &["esc"],
        note: "twice with an empty line to go back to a message or an answer",
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
        id: "lane.next",
        action: A::LaneNext,
        when: W::Editor,
        keys: &["ctrl+o"],
        note: "the next checkout, opening it if it is not",
    },
    Binding {
        id: "app.clear-screen",
        action: A::AppClearScreen,
        when: W::Editor,
        keys: &["ctrl+l"],
        note: "twice to start a new session",
    },
    Binding {
        id: "think.fold",
        action: A::ThinkFold,
        when: W::Editor,
        keys: &["ctrl+t"],
        note: "the current reasoning block in full, or a count of it",
    },
    Binding {
        id: "think.fold-all",
        action: A::ThinkFoldAll,
        when: W::Editor,
        keys: &["ctrl+shift+t", "alt+t"],
        note: "every reasoning block, the current one included",
    },

    // Normal mode from here down. Every key is a bare character, and that is a
    // rule rather than a coincidence: the layer sits above `Editor`, so
    // anything it claims it also takes away — and no binding that existed
    // before vim is a bare character. Bound to one, this layer is pure
    // addition.
    //
    // The ids carry a `normal.` prefix only where one is needed:
    // `move.char.left` is already taken, `mode.insert` cannot be, since the
    // Insert layer is empty and nothing else can ask to leave a mode.
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
        id: "normal.edit.external",
        action: A::EditExternally,
        when: W::Mode(Mode::Normal),
        keys: &["E"],
        note: "the line in $VISUAL/$EDITOR, and back",
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
];

impl Keys {
    /// What is bound right now, as lines. A rebindable system with no way to
    /// see the ids is one nobody can rebind.
    ///
    /// No column says which mode a binding belongs to, because the ids do:
    /// `normal.` is on every one of them that needed telling apart.
    pub fn listing(&self) -> Vec<String> {
        let width = BINDINGS.iter().map(|b| b.id.len()).max().unwrap_or(0);
        BINDINGS
            .iter()
            .map(|b| {
                let mut keys: Vec<String> = self
                    .who
                    .iter()
                    .filter(|(_, id)| **id == b.id)
                    .map(|((_, p), _)| show(*p))
                    .collect();
                keys.sort();
                let note = if b.note.is_empty() {
                    String::new()
                } else {
                    format!("  ·  {}", b.note)
                };
                format!("{:width$}  {}{note}", b.id, keys.join(", "))
            })
            .collect()
    }
}

// A press written the way a config would write it.
fn show(p: Press) -> String {
    let mut out = String::new();
    for (m, name) in [
        (KeyModifiers::CONTROL, "ctrl+"),
        (KeyModifiers::ALT, "alt+"),
        (KeyModifiers::SHIFT, "shift+"),
    ] {
        if p.mods.contains(m) {
            out.push_str(name);
        }
    }
    out.push_str(&match p.code {
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_lowercase(),
    });
    out
}

/// A key press, normalized.
///
/// Shift folds into a bare character rather than being dropped: `A`,
/// `shift+a`, and `a` with the shift bit all arrive as `A`, which is the
/// same press reported three ways depending on the terminal, and stays
/// distinct from `a`. Dropping it instead would collapse the two, and a
/// modal keymap needs `D` to mean something other than `d`. Ctrl and Alt
/// name the unshifted letter, because there the terminals disagree about
/// the character rather than about the modifier — `ctrl+shift+t` stays
/// distinct from `ctrl+t` on the ones that report shift, and degrades to it
/// on the ones that do not, which is why a reachable alternate is worth
/// binding beside it. Named keys keep shift, so `shift+enter` stays
/// expressible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Press {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

impl Press {
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

fn named(word: &str) -> Option<KeyCode> {
    Some(match word {
        "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "esc" | "escape" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        other => {
            let n: u8 = other.strip_prefix('f')?.parse().ok()?;
            if !(1..=12).contains(&n) {
                return None;
            }
            KeyCode::F(n)
        }
    })
}

/// `ctrl+shift+y`, `alt+left`, `f5`, `?`, `D`.
///
/// Modifier names and key names are read case-insensitively; a bare
/// character is not, because `D` and `d` are two presses.
pub fn parse(spec: &str) -> Result<Press> {
    let mut mods = KeyModifiers::NONE;
    let mut rest = spec.trim();
    // Split on the first `+` only while something follows it, so `+` and
    // `ctrl++` name the key itself.
    while let Some((head, tail)) = rest.split_once('+').filter(|(_, t)| !t.is_empty()) {
        match head.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods |= KeyModifiers::CONTROL,
            "alt" | "opt" | "option" | "meta" => mods |= KeyModifiers::ALT,
            "shift" => mods |= KeyModifiers::SHIFT,
            _ => break,
        }
        rest = tail;
    }
    if rest.is_empty() {
        bail!("`{spec}` names no key");
    }
    let code = match named(&rest.to_ascii_lowercase()) {
        Some(c) => c,
        None => {
            let mut chars = rest.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => {
                    bail!("`{spec}` is not a key; try ctrl+w, alt+left, f5, or a single character")
                }
            }
        }
    };
    Ok(Press::of(code, mods))
}

/// Every binding in force, resolved once at startup.
#[derive(Debug)]
pub struct Keys {
    map: HashMap<(When, Press), Action>,
    /// Which binding owns each key. `resolve` builds this to catch conflicts
    /// and it used to be thrown away; keeping it is what lets `listing` ask
    /// "what is bound to this id" instead of reconstructing the answer from
    /// the action, which two ids in one layer are allowed to share.
    who: HashMap<(When, Press), &'static str>,
}

impl Default for Keys {
    fn default() -> Self {
        Self::resolve(&BTreeMap::new()).expect("the built-in table is well formed")
    }
}

impl Keys {
    /// Defaults, with `overrides` replacing the key list of any id it names.
    ///
    /// Replacing rather than adding: a user removing `ctrl+h` from
    /// delete-char-back has no other way to say so, and an override that could
    /// only add would make the defaults permanent. An explicit binding also
    /// wins over a default on the same key — that is how a config that copied
    /// the old defaults keeps working when a default splits into two ids —
    /// and only two explicit bindings on one key are an error.
    pub fn resolve(overrides: &BTreeMap<String, Vec<String>>) -> Result<Self> {
        for id in overrides.keys() {
            if !BINDINGS.iter().any(|b| b.id == id) {
                let known: Vec<&str> = BINDINGS.iter().map(|b| b.id).collect();
                bail!("unknown key action `{id}`; known: {}", known.join(", "));
            }
        }
        let mut map: HashMap<(When, Press), Action> = HashMap::new();
        let mut who: HashMap<(When, Press), &str> = HashMap::new();
        // Explicit bindings first: they are authoritative over defaults, and
        // two of them on one key in one context is a genuine conflict.
        for b in BINDINGS {
            if let Some(v) = overrides.get(b.id) {
                for spec in v {
                    let press = parse(spec).map_err(|e| anyhow::anyhow!("{}: {e}", b.id))?;
                    if let Some(other) = who.insert((b.when, press), b.id) {
                        bail!("`{spec}` is bound to both {other} and {} at once", b.id);
                    }
                    map.insert((b.when, press), b.action);
                }
            }
        }
        // Defaults fill what the user has not claimed; one that lands on an
        // explicitly bound key yields, and one that repeats another default
        // is a table bug the same error catches.
        for b in BINDINGS {
            if overrides.contains_key(b.id) {
                continue;
            }
            for spec in b.keys {
                let press = parse(spec).map_err(|e| anyhow::anyhow!("{}: {e}", b.id))?;
                if let Some(other) = who.get(&(b.when, press)) {
                    if overrides.contains_key(*other) {
                        continue;
                    }
                    bail!("`{spec}` is bound to both {other} and {} at once", b.id);
                }
                who.insert((b.when, press), b.id);
                map.insert((b.when, press), b.action);
            }
        }
        Ok(Self { map, who })
    }

    /// What this press means, given what is on screen.
    pub fn action(&self, press: Press, layers: Layers) -> Option<Action> {
        let mut live = Vec::with_capacity(4);
        if layers.menu {
            live.push(When::Menu);
        }
        if layers.run {
            live.push(When::Run);
        }
        if let Some(mode) = layers.mode {
            live.push(When::Mode(mode));
        }
        live.push(When::Editor);
        if let hit @ Some(_) = live
            .iter()
            .find_map(|w| self.map.get(&(*w, press)).copied())
        {
            return hit;
        }
        // A shift-riding press that nothing owns — `ctrl+shift+t` has its
        // own binding, but the terminals that do not report shift make
        // `ctrl+shift+w` arrive as `ctrl+w`. Falling back to the bare press
        // keeps those keys working on both kinds of terminal.
        if press.mods.contains(KeyModifiers::SHIFT) {
            let bare = Press {
                mods: press.mods - KeyModifiers::SHIFT,
                ..press
            };
            return live
                .iter()
                .find_map(|w| self.map.get(&(*w, bare)).copied());
        }
        None
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn folding_the_reasoning_is_reachable_while_a_run_is_in_flight() {
        // The window is only worth having while the reasoning is arriving, so
        // a binding that resolved between turns and not during them would be
        // the one context it is useless in.
        let keys = Keys::resolve(&BTreeMap::new()).unwrap();
        for (menu, running) in [(false, false), (false, true), (true, true)] {
            assert_eq!(
                keys.action(press("ctrl+t"), Layers { menu, run: running, ..Layers::default() }),
                Some(Action::ThinkFold),
                "menu={menu} running={running}"
            );
        }
    }
    use super::*;

    fn press(s: &str) -> Press {
        parse(s).unwrap()
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
            k.action(press("up"), Layers { menu: true, ..Layers::default() }),
            Some(Action::MenuPrevious)
        );
        assert_eq!(
            k.action(press("up"), Layers::default()),
            Some(Action::HistoryOlder)
        );
        assert_eq!(
            k.action(press("esc"), Layers { menu: true, run: true, mode: None }),
            Some(Action::MenuDismiss)
        );
        assert_eq!(k.action(press("esc"), Layers::default()), Some(Action::Rewind));
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

    #[test]
    fn the_normal_layer_takes_nothing_away() {
        // The premise the whole table rests on: Normal binds bare characters,
        // and nothing bound before vim existed is one, so every older binding
        // still answers under it. A Normal key on a modifier combination would
        // quietly shadow one — the layer sits above `Editor` — and this is the
        // only thing that would notice.
        let k = Keys::default();
        for b in BINDINGS {
            if b.when == W::Mode(Mode::Normal) {
                continue;
            }
            for spec in b.keys {
                let press = parse(spec).unwrap();
                let insert = Layers {
                    menu: b.when == W::Menu,
                    run: b.when == W::Run,
                    mode: None,
                };
                assert_eq!(
                    k.action(press, insert),
                    k.action(press, Layers { mode: Some(Mode::Normal), ..insert }),
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
        assert_eq!(k.action(press("h"), Layers::default()), None);
        assert_eq!(
            k.action(press("h"), Layers { mode: Some(Mode::Normal), ..Layers::default() }),
            Some(Action::MoveCharLeft)
        );
    }

    #[test]
    fn normal_tells_a_capital_from_its_lowercase() {
        // What the shift-folding change bought, spent: `x` and `X` delete in
        // two directions, and before it they were one press.
        let k = Keys::default();
        let normal = Layers { mode: Some(Mode::Normal), ..Layers::default() };
        assert_eq!(k.action(press("x"), normal), Some(Action::DeleteCharForward));
        assert_eq!(k.action(press("X"), normal), Some(Action::DeleteCharBack));
        assert_eq!(k.action(press("i"), normal), Some(Action::ModeInsert));
        assert_eq!(k.action(press("I"), normal), Some(Action::ModeInsertLineStart));
    }

    #[test]
    fn the_listing_keeps_two_ids_that_share_an_action_apart() {
        // `move.char.left` and `normal.move.char.left` are one action under
        // two ids. A listing that found a binding's keys by its action would
        // print `h, left` against both, and neither could be rebound alone.
        let lines = Keys::default().listing();
        let keys_of = |id: &str| {
            let line = lines
                .iter()
                .find(|l| l.starts_with(&format!("{id} ")))
                .unwrap_or_else(|| panic!("{id} is not listed"));
            line[id.len()..].trim().to_string()
        };
        assert_eq!(keys_of("move.char.left"), "left");
        assert_eq!(keys_of("normal.move.char.left"), "h");
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
        // view.scroll-up used to default to ["pageup", "ctrl+b"]; after the
        // half-page split its ctrl+b would collide with the new half-up
        // default, and the explicit copy must win, not error.
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
        // Terminals disagree about whether shift+a arrives as Char('A'),
        // Char('A') with SHIFT, or Char('a') with SHIFT. All three are one
        // press and must converge; a table that distinguished them would work
        // on some terminals and not others.
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
        // The payoff: before shift folded into the character these collided
        // as one press, and a table wanting both had to give one of them up.
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
    fn a_capital_survives_the_round_trip_through_the_listing() {
        // `/keys` prints presses with `show`, and what it prints has to be
        // what a config can type back in.
        for spec in ["D", "d", "ctrl+shift+t", "shift+enter", "$", "f5"] {
            assert_eq!(press(&show(press(spec))), press(spec), "{spec}");
        }
    }
    #[test]
    fn a_shift_riding_press_falls_back_to_the_bare_key() {
        // `ctrl+shift+w` is its own press only on the terminals that report
        // the shift; on the ones that swallow it, it is `ctrl+w`. The lookup
        // falls back, so the key does the same thing everywhere.
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
        // Ctrl+Shift+T is ctrl+t on terminals that swallow the shift; on the
        // ones that report it, it must reach the action it is bound to. Alt+T
        // stays bound too, so the degrade has a reachable alternate.
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
    fn a_plus_is_a_key_like_any_other() {
        assert_eq!(press("+").code, KeyCode::Char('+'));
        assert_eq!(
            press("ctrl++"),
            Press::of(KeyCode::Char('+'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn nonsense_is_refused_with_a_hint() {
        assert!(parse("").is_err());
        assert!(parse("ctrl+").is_err());
        assert!(parse("f13").is_err());
        let e = parse("ctrl+nope").unwrap_err().to_string();
        assert!(e.contains("try ctrl+w"), "{e}");
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
    #[test]
    fn the_listing_writes_keys_the_way_a_config_would() {
        let rows = Keys::default().listing().join("\n");
        assert!(rows.contains("edit.delete.word-back"), "{rows}");
        // Round-trips: the keys shown can be pasted back into [keys].
        for row in Keys::default().listing() {
            let keys = row.split("  ·  ").next().unwrap();
            let keys = keys.split_once("  ").expect("id then keys").1;
            for spec in keys.trim().split(", ") {
                assert!(parse(spec).is_ok(), "cannot re-read `{spec}`");
            }
        }
    }
}
