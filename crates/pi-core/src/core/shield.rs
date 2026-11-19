//! What this run must not send: the values the config and the tool scripts
//! name, and the key that names them back.

use std::path::{Path, PathBuf};

use llm::shield::{Footprint, Tier};
use pi_store::config;
use pi_store::listing::{Listing, Row};

/// Below this a value matches so much of a transcript that masking it costs
/// more than the leak it prevents.
const MIN: usize = 8;

/// What one resolution found: the key that names the values, and the values
/// themselves as `(kind, tier, value)`.
pub struct Found {
    pub key: [u8; 32],
    pub values: Vec<(String, Tier, String)>,
}

/// Everything this workspace and configuration name, or `None` when there is
/// no pi directory to keep the key in.
pub fn find(workspace: &Path, config: &config::Config) -> Option<Found> {
    let dirs: Vec<PathBuf> = (super::tools::dirs(workspace, "tools"))
        .into_iter()
        .map(|(dir, _)| dir)
        .collect();
    Some(Found {
        key: pi_store::shield::key()?,
        values: values_of(&dirs, config),
    })
}

// What the owner named is taken as given, however short: it is their
// decision. A `.env` holds settings beside keys, so only what looks like one
// is taken from there.
fn values_of(dirs: &[PathBuf], config: &config::Config) -> Vec<(String, Tier, String)> {
    let mut values: Vec<(String, Tier, String)> = Vec::new();
    for value in config.keys() {
        values.push(("API_KEY".to_string(), Tier::Named, value));
    }
    for value in config.secrets() {
        values.push(("SECRET".to_string(), Tier::Named, value));
    }
    for dir in dirs {
        for (name, value) in pi_tools::scripts::env::read(dir) {
            if shapes_a_secret(&value) {
                values.push((label_of(&name), Tier::Guessed, value));
            }
        }
    }
    values
}

// A `.env` holds settings beside keys, so a value has to look like one: mixed
// letters and digits, or long enough that no word reaches it.
fn shapes_a_secret(value: &str) -> bool {
    if value.len() < MIN {
        return false;
    }
    let digit = value.chars().any(|c| c.is_ascii_digit());
    let letter = value.chars().any(|c| c.is_ascii_alphabetic());
    (digit && letter) || value.len() >= 16
}

// The kind a variable's own name says it is, so the token reads `{{PI_TOKEN_…}}`.
fn label_of(name: &str) -> String {
    let last = name.rsplit('_').next().unwrap_or(name).to_ascii_uppercase();
    match last.as_str() {
        "KEY" | "TOKEN" | "SECRET" | "PASSWORD" | "PASSWD" | "CREDENTIALS" => last,
        _ => "SECRET".to_string(),
    }
}

/// What this run has masked, as the surface shows it: the token the vendor
/// saw, the head of the value, and whether the mask rested on a guess.
pub fn listing() -> Listing {
    listing_of(super::dial::footprints())
}

fn listing_of(prints: Option<Vec<Footprint>>) -> Listing {
    match prints {
        // No key, so nothing is masked at all: saying "nothing masked" would
        // read as "the run had nothing to hide".
        None => Listing::say(["masking is off — there is no key to name values by"]),
        Some(prints) if prints.is_empty() => Listing::say([
            "nothing to mask — no key, no listed secret and no script credential was found",
        ]),
        Some(prints) => rows(prints),
    }
}

fn rows(prints: Vec<Footprint>) -> Listing {
    Listing::of(prints.into_iter().map(|print| {
        let note = format!(
            "{} chars · {}× · {}",
            print.len,
            print.hits,
            match print.tier {
                Tier::Named => "from the config",
                Tier::Guessed => "by shape",
            }
        );
        let head = match print.head.chars().count() < print.len {
            true => format!("{}…", print.head),
            false => print.head,
        };
        Row::new([print.token, head]).noting(note)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::shield::Shield;

    fn config(body: &str) -> config::Config {
        config::Config::in_force(toml::from_str(body).expect("a tree")).expect("a config")
    }

    // The listing is the only place a person sees what was masked: the head
    // of each value, marked only when there is more of it than is shown, and
    // how often it left.
    #[test]
    fn the_listing_shows_a_token_its_head_and_how_often_it_left() {
        let mut shield = Shield::new([7; 32]);
        shield.add("SECRET", "hunter2-the-password");
        shield.add("API_KEY", "short-key");
        let _ = shield.cover("hunter2-the-password");

        let prints = shield.footprints();
        let listing = rows(prints.clone());
        assert_eq!(listing.rows.len(), 2);
        for (row, print) in listing.rows.iter().zip(&prints) {
            assert!(row.cells[1].starts_with(&print.head), "{row:?}");
            let shown_less = row.cells[1].chars().count() > print.head.chars().count();
            assert_eq!(shown_less, print.len > print.head.chars().count());
        }
        let note = listing.rows[0].note.clone().expect("a note");
        assert!(note.contains("20 chars"), "{note}");
        assert!(note.contains("1×"), "{note}");
        assert!(note.contains("from the config"), "{note}");
    }

    // A run with no key masks nothing at all, which is not the same answer as
    // a run that had nothing to mask, and neither is an empty list.
    #[test]
    fn a_run_with_nothing_armed_and_a_run_with_nothing_to_mask_read_differently() {
        let said = |listing: &Listing| {
            listing
                .rows
                .iter()
                .flat_map(|row| row.cells.iter())
                .cloned()
                .collect::<String>()
        };
        let off = listing_of(None);
        let empty = listing_of(Some(Vec::new()));
        assert_eq!(off.rows.len(), 1);
        assert_eq!(empty.rows.len(), 1);
        assert_ne!(said(&off), said(&empty));
    }

    // A connection's key, what the user listed and a script's `.env` all reach
    // one shield; a value too plain or too short to be a secret stays as it
    // was, so the model still reads its text.
    #[test]
    fn the_values_the_config_and_the_scripts_name_are_covered() {
        let workspace = tempfile::tempdir().expect("a workspace");
        let tools = workspace.path().join(".pi/tools");
        std::fs::create_dir_all(&tools).expect("the tools directory");
        std::fs::write(
            tools.join(".env"),
            "TELEGRAM_BOT_TOKEN=123456:AAbbCCddEEffGG\nMODE=production\nDEBUG=1\n",
        )
        .expect("the env file");
        let config = config(
            "secrets = [\"hunter2-the-password\"]\n\
             [provider.up]\nbase_url = \":3425/v1\"\nformat = \"chat\"\n\
             api_key = \"sk-up-0123456789\"\n",
        );

        let mut shield = Shield::new([7; 32]);
        for (kind, tier, value) in values_of(std::slice::from_ref(&tools), &config) {
            match tier {
                Tier::Named => shield.add(&kind, &value),
                Tier::Guessed => shield.add_guessed(&kind, &value),
            }
        }
        let text = "key sk-up-0123456789 bot 123456:AAbbCCddEEffGG \
                    pass hunter2-the-password mode production debug 1";
        let covered = shield.cover(text);
        assert!(!covered.contains("sk-up-0123456789"), "{covered}");
        assert!(!covered.contains("AAbbCCddEEffGG"), "{covered}");
        assert!(!covered.contains("hunter2-the-password"), "{covered}");
        assert!(covered.contains("mode production"), "{covered}");
        assert!(covered.contains("debug 1"), "{covered}");
        assert_eq!(shield.restore(&covered), text);
    }
}
