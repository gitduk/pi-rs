//! The textual form of a key press: what `ctrl+shift+y` means, and how a
//! binding is written back out.
//!
//! Second half of `mod.rs`: that says what the table is, this says how a key
//! is spelled in it — the config file's side, and the `/keys` listing's.

use super::{Action, BINDINGS, Chord, Key, Keys, Layers, Mode, Mods, On, Press, When};
use crate::listing::{Listing, Row};
use anyhow::{Result, bail};

impl Keys {
    /// What is bound right now: the id it is rebound by, the keys that reach
    /// it, and when and what it does.
    pub fn listing(&self) -> Listing {
        Listing::of(BINDINGS.iter().map(|b| {
            let row = Row::new([b.id.to_string(), self.keys_of(b.id).join(", ")]);
            let note = [live_when(b.when), b.note.to_string()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(": ");
            match note.is_empty() {
                true => row,
                false => row.noting(&note),
            }
        }))
    }

    /// The keys that do `action` with `layers` up, as a config writes them,
    /// shortest first: each read as a press is, so one a nearer or narrower
    /// binding takes is not among them.
    pub fn live_keys(&self, action: Action, layers: Layers) -> Vec<String> {
        let mut keys: Vec<String> = (self.map.iter())
            .filter(|(_, bound)| **bound == action)
            .map(|((_, chord), _)| *chord)
            .filter(|chord| {
                let (prev, press, pair) = match *chord {
                    Chord::One(p) => (None, p, false),
                    Chord::Two(a, p) => (Some(a), p, true),
                };
                self.hit(prev, press, layers) == Some(super::Hit { action, pair })
            })
            .map(show_chord)
            .collect();
        keys.sort_by_key(|k| (crate::text::width(k), k.clone()));
        keys.dedup();
        keys
    }

    /// The keys bound to `id` now, as a config writes them, sorted.
    pub fn keys_of(&self, id: &str) -> Vec<String> {
        let mut keys: Vec<String> = (self.who.iter())
            .filter(|(_, bound)| **bound == id)
            .map(|((_, c), _)| show_chord(*c))
            .collect();
        keys.sort();
        keys
    }
}

// When a binding is live, as `/keys` says it; nothing for the line in
// either mode. Insert goes unsaid: with vim off it is the only mode.
fn live_when(when: When) -> String {
    let on = match when.on {
        On::List => "over a list",
        On::Page => "over a page or a search",
        On::Run => "while a turn runs",
        On::Line => "",
        On::App => "anywhere",
    };
    let mode = match when.mode {
        Some(Mode::Normal) => "in Normal",
        Some(Mode::Insert) | None => "",
    };
    let empty = if when.empty { "on an empty line" } else { "" };
    [mode, empty, on]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

// A chord written the way a config would write it: presses apart by a space.
fn show_chord(c: Chord) -> String {
    match c {
        Chord::One(p) => show(p),
        Chord::Two(a, b) => format!("{} {}", show(a), show(b)),
    }
}

// Every key spelled by name, its canonical spelling first. Reading and `show`
// both go through here, so what `/keys` prints is what a config can type back.
const NAMES: &[(Key, &[&str])] = &[
    (Key::Enter, &["enter", "return"]),
    (Key::Tab, &["tab"]),
    (Key::BackTab, &["backtab"]),
    (Key::Backspace, &["backspace"]),
    (Key::Delete, &["delete", "del"]),
    (Key::Insert, &["insert"]),
    (Key::Esc, &["esc", "escape"]),
    (Key::Char(' '), &["space"]),
    (Key::Up, &["up"]),
    (Key::Down, &["down"]),
    (Key::Left, &["left"]),
    (Key::Right, &["right"]),
    (Key::Home, &["home"]),
    (Key::End, &["end"]),
    (Key::PageUp, &["pageup"]),
    (Key::PageDown, &["pagedown"]),
];

// A press written the way a config would write it.
fn show(p: Press) -> String {
    let mut out = String::new();
    for (on, name) in [
        (p.mods.ctrl, "ctrl+"),
        (p.mods.alt, "alt+"),
        (p.mods.shift, "shift+"),
    ] {
        if on {
            out.push_str(name);
        }
    }
    let name = NAMES.iter().find(|(k, _)| *k == p.code);
    out.push_str(&match (name, p.code) {
        (Some((_, names)), _) => names[0].to_string(),
        (None, Key::Char(c)) => c.to_string(),
        (None, Key::F(n)) => format!("f{n}"),
        (None, _) => "unnamed".to_string(),
    });
    out
}

fn named(word: &str) -> Option<Key> {
    if let Some((key, _)) = NAMES.iter().find(|(_, names)| names.contains(&word)) {
        return Some(*key);
    }
    let n: u8 = word.strip_prefix('f')?.parse().ok()?;
    (1..=12).contains(&n).then_some(Key::F(n))
}

/// One press, or two apart by a space: `ctrl+w`, `g g`, `ctrl+c ctrl+c`.
pub fn chord(spec: &str) -> Result<Chord> {
    let presses: Vec<&str> = spec.split_whitespace().collect();
    match presses.as_slice() {
        [one] => Ok(Chord::One(parse(one)?)),
        [a, b] => Ok(Chord::Two(parse(a)?, parse(b)?)),
        [] => bail!("`{spec}` names no key"),
        _ => bail!("`{spec}` is more than two presses; a binding is one press or a pair"),
    }
}

/// `ctrl+shift+y`, `alt+left`, `f5`, `?`, `D`.
///
/// Modifier names and key names are read case-insensitively; a bare
/// character is not, because `D` and `d` are two presses.
pub fn parse(spec: &str) -> Result<Press> {
    let mut mods = Mods::NONE;
    let mut rest = spec.trim();
    // Split on the first `+` only while something follows it, so `+` and
    // `ctrl++` name the key itself.
    while let Some((head, tail)) = rest.split_once('+').filter(|(_, t)| !t.is_empty()) {
        match head.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "opt" | "option" | "meta" => mods.alt = true,
            "shift" => mods.shift = true,
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
                (Some(c), None) => Key::Char(c),
                _ => {
                    bail!("`{spec}` is not a key; try ctrl+w, alt+left, f5, or a single character")
                }
            }
        }
    };
    Ok(Press::of(code, mods))
}

#[cfg(test)]
mod tests {
    use super::super::tests::press;
    use super::{Key, Mods, NAMES, Press, parse, show};
    #[test]
    fn a_capital_survives_the_round_trip_through_the_listing() {
        // `/keys` prints presses with `show`, and what it prints has to be
        // what a config can type back in.
        for spec in ["D", "d", "ctrl+shift+t", "shift+enter", "$", "f5"] {
            assert_eq!(press(&show(press(spec))), press(spec), "{spec}");
        }
    }
    #[test]
    fn every_named_key_reads_back_from_what_is_shown() {
        let keys = NAMES.iter().map(|(k, _)| *k).chain((1..=12).map(Key::F));
        for key in keys {
            for (ctrl, alt, shift) in [
                (false, false, false),
                (true, false, true),
                (false, true, false),
            ] {
                let p = Press::of(key, Mods { ctrl, alt, shift });
                assert_eq!(press(&show(p)), p, "{p:?} shown as {}", show(p));
            }
        }
        for (key, names) in NAMES {
            for name in *names {
                assert_eq!(press(name).code, *key, "{name}");
            }
        }
    }
    #[test]
    fn a_plus_is_a_key_like_any_other() {
        assert_eq!(press("+").code, Key::Char('+'));
        let ctrl = Mods {
            ctrl: true,
            ..Mods::NONE
        };
        assert_eq!(press("ctrl++"), Press::of(Key::Char('+'), ctrl));
    }
    #[test]
    fn nonsense_is_refused_with_a_hint() {
        assert!(parse("").is_err());
        assert!(parse("ctrl+").is_err());
        assert!(parse("f13").is_err());
        let e = parse("ctrl+nope").unwrap_err().to_string();
        assert!(e.contains("try ctrl+w"), "{e}");
    }
}
