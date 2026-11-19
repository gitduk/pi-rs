//! What this run must not send: the values the config and the tool scripts
//! name, and the key that names them back.

use std::path::{Path, PathBuf};

use llm::mask::{Footprint, Tier};
use pi_store::config;
use pi_store::listing::{Listing, Role, Row};
use std::collections::BTreeMap;

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
        key: pi_store::mask::key()?,
        values: values_of(&dirs, config),
    })
}

// A value this short matches more prose than secret: a stub key would cover
// every word that holds it.
fn long_enough(value: &str) -> bool {
    value.chars().count() >= MIN
}

fn values_of(dirs: &[PathBuf], config: &config::Config) -> Vec<(String, Tier, String)> {
    let mut values: Vec<(String, Tier, String)> = Vec::new();
    for value in config.keys() {
        if long_enough(&value) {
            values.push(("API_KEY".to_string(), Tier::Named, value));
        }
    }
    // A listed secret is the owner saying it must not leave, whatever it
    // looks like: the shape rules are why the list exists.
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
    if !long_enough(value) {
        return false;
    }
    let digit = value.chars().any(|c| c.is_ascii_digit());
    let letter = value.chars().any(|c| c.is_ascii_alphabetic());
    (digit && letter) || value.chars().count() >= 16
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
/// saw, the two ends of the value, and whether the mask rested on a guess.
pub fn listing() -> Listing {
    listing_of(super::dial::footprints())
}

fn listing_of(prints: Option<Vec<Footprint>>) -> Listing {
    match prints {
        // No key, so nothing is masked at all: saying "nothing masked" would
        // read as "the run had nothing to hide".
        None => Listing::say(["masking is off — there is no key to name values by"]),
        Some(prints) if prints.is_empty() => Listing::say([
            "nothing to mask — no key long enough, no listed secret and no script credential \
             was found",
        ]),
        Some(prints) => rows(prints),
    }
}

fn rows(prints: Vec<Footprint>) -> Listing {
    let sent: u64 = prints.iter().map(|print| print.hits).sum();
    let values = prints.len();
    // One table per kind: a run's keys, its secrets and its script tokens are
    // read for different reasons, and each group stays small enough to sit in
    // a column of its own.
    let mut kinds: BTreeMap<String, Vec<Footprint>> = BTreeMap::new();
    for print in prints {
        kinds.entry(print.kind.clone()).or_default().push(print);
    }
    let mut kinds: Vec<_> = kinds.into_iter().collect();
    kinds.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| a.0.cmp(&b.0)));
    let mut rows = vec![
        Row::new(["Mask"]).with_role(Role::Title),
        Row::new([
            "values".into(),
            values.to_string(),
            "masked".into(),
            format!("{sent}× sent"),
        ])
        .with_role(Role::Metric),
    ];
    for (kind, mut prints) in kinds {
        // What left most stands first: that is what a reader came to see.
        prints.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| a.token.cmp(&b.token)));
        // A token's length follows the kind it names, so the column is padded
        // here and the ends behind it start at one column across rows.
        let widest = pi_store::text::widest(prints.iter().map(Footprint::bare));
        rows.push(Row::new([kind.as_str(), "BY", "CHARS", "SENT"]).with_role(Role::Section));
        rows.extend(prints.into_iter().map(|print| {
            Row::new([
                pi_store::text::pad(print.bare(), widest),
                match print.tier {
                    Tier::Named => "config".to_string(),
                    Tier::Guessed => "shape".to_string(),
                },
                print.len.to_string(),
                print.hits.to_string(),
            ])
            .noting(ends(&print))
            .with_role(Role::Budget)
        }));
    }
    Listing::report(rows)
}

// The two ends of a value, marked as cut when there is more of it than shown.
fn ends(print: &Footprint) -> String {
    let shown = print.head.chars().count() + print.tail.chars().count();
    // Both ends only while they stay a minority of the value: two thirds of a
    // short secret is enough to guess the rest of it.
    match shown * 2 <= print.len {
        true => format!("{}…{}", print.head, print.tail),
        false => print.head.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::mask::Mask;

    fn config(body: &str) -> config::Config {
        config::Config::in_force(toml::from_str(body).expect("a tree")).expect("a config")
    }

    // The listing is the only place a person sees what was masked: the two
    // ends of each value, marked only when there is more of it than is shown,
    // why it was taken and how often it left.
    #[test]
    fn the_listing_shows_both_ends_why_and_how_often_it_left() {
        let mut mask = Mask::new([7; 32]);
        mask.add("SECRET", "hunter2-the-password");
        mask.add("API_KEY", "short-key");
        // Twelve characters: its two ends would be two thirds of it.
        mask.add("SECRET", "abcdefghijkl");
        let _ = mask.cover("hunter2-the-password");

        let prints = mask.footprints();
        let listing = rows(prints.clone());
        let cells: Vec<&Row> = listing
            .rows
            .iter()
            .filter(|row| row.role == Role::Budget)
            .collect();
        assert_eq!(cells.len(), prints.len());
        for print in &prints {
            let row = cells
                .iter()
                .find(|row| row.cells[0].trim_end() == print.bare())
                .expect("a row per value");
            let note = row.note.as_deref().expect("a note");
            assert!(!row.cells[0].contains("{{"), "{row:?}");
            assert!(note.starts_with(&print.head), "{row:?}");
            assert_eq!(row.cells[1], "config", "{row:?}");
            assert_eq!(row.cells[2], print.len.to_string(), "{row:?}");
            assert_eq!(row.cells[3], print.hits.to_string(), "{row:?}");
        }
        let long = prints
            .iter()
            .find(|print| print.len > print.head.chars().count() + print.tail.chars().count())
            .expect("a value longer than its ends");
        let row = cells
            .iter()
            .find(|row| row.cells[0].trim_end() == long.bare())
            .expect("its row");
        let note = row.note.as_deref().unwrap();
        assert!(note.contains('\u{2026}'), "{row:?}");
        // Both ends while the hidden middle is the longer half of the value.
        assert!(note.ends_with(&long.tail), "{row:?}");
        // A value those two ends would give away shows its head alone.
        let short = prints
            .iter()
            .find(|print| print.len < 2 * (print.head.chars().count() + print.tail.chars().count()))
            .expect("a value shorter than twice its ends");
        let row = cells
            .iter()
            .find(|row| row.cells[0].trim_end() == short.bare())
            .expect("its row");
        assert_eq!(row.note.as_deref(), Some(short.head.as_str()), "{row:?}");

        let summary = listing
            .rows
            .iter()
            .find(|row| row.role == Role::Metric)
            .expect("a summary");
        assert_eq!(summary.cells[1], prints.len().to_string(), "{summary:?}");
        let sent: u64 = prints.iter().map(|print| print.hits).sum();
        assert!(
            summary.cells[3].starts_with(&sent.to_string()),
            "{summary:?}"
        );
    }

    // A connection's stub key is no secret and is left alone; a listed secret
    // is the owner's word however short it is, and the floor keeps its edge.
    #[test]
    fn a_stub_key_is_skipped_while_a_listed_secret_is_taken_as_written() {
        let kept = "k".repeat(MIN);
        let config = config(&format!(
            "secrets = [\"abcd\"]\n\
             [provider.up]\nbase_url = \":3425/v1\"\nformat = \"chat\"\napi_key = \"z\"\n\
             [provider.down]\nbase_url = \":3426/v1\"\nformat = \"chat\"\napi_key = \"{kept}\"\n",
        ));
        assert_eq!(
            values_of(&[], &config),
            vec![
                ("API_KEY".to_string(), Tier::Named, kept),
                ("SECRET".to_string(), Tier::Named, "abcd".to_string()),
            ]
        );
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
    // one mask; a value too plain or too short to be a secret stays as it
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

        let mut mask = Mask::new([7; 32]);
        for (kind, tier, value) in values_of(std::slice::from_ref(&tools), &config) {
            match tier {
                Tier::Named => mask.add(&kind, &value),
                Tier::Guessed => mask.add_guessed(&kind, &value),
            }
        }
        let text = "key sk-up-0123456789 bot 123456:AAbbCCddEEffGG \
                    pass hunter2-the-password mode production debug 1";
        let covered = mask.cover(text);
        assert!(!covered.contains("sk-up-0123456789"), "{covered}");
        assert!(!covered.contains("AAbbCCddEEffGG"), "{covered}");
        assert!(!covered.contains("hunter2-the-password"), "{covered}");
        assert!(covered.contains("mode production"), "{covered}");
        assert!(covered.contains("debug 1"), "{covered}");
        assert_eq!(mask.restore(&covered), text);
    }
}
