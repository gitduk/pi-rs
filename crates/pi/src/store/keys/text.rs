//! The textual form of a key press: what `ctrl+shift+y` means, and how a
//! binding is written back out.
//!
//! Second half of the module next door: `mod.rs` says what the table is,
//! this says how a key is spelled in it — the config file's side, and the
//! `/keys` listing's.

use anyhow::{Result, bail};
use crossterm::event::{KeyCode, KeyModifiers};

use super::{BINDINGS, Keys, Press};
use crate::store::listing::{Listing, Row};

impl Keys {
    /// What is bound right now: the id it is rebound by, the keys that reach
    /// it, and what it does. A rebindable system with no way to see the ids is
    /// one nobody can rebind.
    ///
    /// No cell says which mode a binding belongs to, because the ids do:
    /// `normal.` is on everyone that needed telling apart.
    pub fn listing(&self) -> Listing {
        Listing::of(BINDINGS.iter().map(|b| {
            let mut keys: Vec<String> = self
                .who
                .iter()
                .filter(|(_, id)| **id == b.id)
                .map(|((_, p), _)| show(*p))
                .collect();
            keys.sort();
            let row = Row::new([b.id.to_string(), keys.join(", ")]);
            match b.note.is_empty() {
                true => row,
                false => row.noting(b.note),
            }
        }))
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

#[cfg(test)]
mod tests {
    use super::super::tests::press;
    use super::{Keys, Press, parse, show};
    use crossterm::event::{KeyCode, KeyModifiers};
    #[test]
    fn a_capital_survives_the_round_trip_through_the_listing() {
        // `/keys` prints presses with `show`, and what it prints has to be
        // what a config can type back in.
        for spec in ["D", "d", "ctrl+shift+t", "shift+enter", "$", "f5"] {
            assert_eq!(press(&show(press(spec))), press(spec), "{spec}");
        }
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
    fn the_listing_writes_keys_the_way_a_config_would() {
        let rows = crate::ui::listing::lines(&Keys::default().listing()).join("\n");
        assert!(rows.contains("edit.delete.word-back"), "{rows}");
        // Round-trips: the keys shown can be pasted back into [keys].
        for row in crate::ui::listing::lines(&Keys::default().listing()) {
            let keys = row.split(crate::store::icons::KEY_NOTE_SEP).next().unwrap();
            let keys = keys.split_once("  ").expect("id then keys").1;
            for spec in keys.trim().split(", ") {
                assert!(parse(spec).is_ok(), "cannot re-read `{spec}`");
            }
        }
    }
}
