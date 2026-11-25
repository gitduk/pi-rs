//! What this run must not send: the values the config and the tool scripts
//! name, and the key that names them back.

use std::path::{Path, PathBuf};

use llm::mask::{Footprint, Mask, Tier};
use pi_store::config;
use pi_store::listing::{Listing, Role, Row};
use pi_store::text::{clip, pad, widest};

/// Below this a value matches so much of a transcript that masking it costs
/// more than the leak it prevents.
const MIN: usize = 8;

/// Length at which the shape heuristic accepts a value without mixed letters and digits.
const LONG_SECRET_MIN: usize = 16;

/// Widest a name column gets before it is cut: room for
/// `provider.<name>.api_key`, short enough that the value still fits on the
/// line beside it.
const NAME_MAX: usize = 22;

/// One value this run must not send, how it was named, and how it was recognised.
#[derive(Debug, PartialEq)]
pub struct Secret {
    pub name: Option<String>,
    pub tier: Tier,
    pub value: String,
}

impl Secret {
    /// Register this value with `mask`, under the name that introduced it.
    pub(super) fn cover_into(&self, mask: &mut Mask) {
        match self.tier {
            Tier::Named => mask.add(self.name.as_deref(), &self.value),
            Tier::Guessed => mask.add_guessed(self.name.as_deref(), &self.value),
        }
    }
}

/// What one resolution found: the key that names the values, and the values
/// themselves.
pub struct Found {
    pub key: [u8; 32],
    pub values: Vec<Secret>,
}

/// Everything this workspace and configuration name, or `None` when there is
/// no pi directory to keep the key in.
pub fn find(workspace: &Path, config: &config::Config) -> Option<Found> {
    let key = pi_store::mask::key()?;
    let dirs = super::tools::dirs(workspace, "tools")
        .into_iter()
        .map(|(dir, _)| dir);
    Some(Found {
        key,
        values: values_of(dirs, config),
    })
}

// A value this short matches more prose than secret: a stub key would cover
// every word that holds it.
fn long_enough(value: &str, floor: usize) -> bool {
    value.chars().take(floor).count() == floor
}

fn values_of(dirs: impl IntoIterator<Item = PathBuf>, config: &config::Config) -> Vec<Secret> {
    let mut values: Vec<Secret> = Vec::new();
    for credential in config.keys() {
        if long_enough(&credential.value, MIN) {
            values.push(Secret {
                name: Some(credential.name),
                tier: Tier::Named,
                value: credential.value,
            });
        }
    }
    // A listed secret is the owner saying it must not leave, whatever it
    // looks like: the shape rules are why the list exists.
    for value in config.secrets() {
        values.push(Secret {
            name: None,
            tier: Tier::Named,
            value,
        });
    }
    let env_values = dirs
        .into_iter()
        .flat_map(|dir| pi_store::envfile::read(&dir));
    for (name, value) in env_values {
        // An explicit name halves the minimum length needed for masking.
        if !long_enough(&value, MIN / 2) {
            continue;
        }
        let tier = if names_a_credential(&name) {
            Tier::Named
        } else if shapes_a_secret(&value) {
            Tier::Guessed
        } else {
            continue;
        };
        values.push(Secret {
            name: Some(name),
            tier,
            value,
        });
    }
    values
}

fn shapes_a_secret(value: &str) -> bool {
    if let Some((scheme, _)) = value.split_once("://")
        && let Ok(url) = reqwest::Url::parse(value)
        && url.scheme().eq_ignore_ascii_case(scheme)
    {
        // Keep ordinary base URLs visible. Userinfo, queries, and fragments
        // can carry credentials even when the variable name is generic.
        return !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some();
    }

    let length = value.chars().take(LONG_SECRET_MIN).count();
    length >= LONG_SECRET_MIN
        || (length >= MIN
            && value.bytes().any(|byte| byte.is_ascii_digit())
            && value.bytes().any(|byte| byte.is_ascii_alphabetic()))
}

// Credential suffixes count as explicit naming in a `.env` file.
fn names_a_credential(name: &str) -> bool {
    let suffix = name.rsplit_once('_').map_or(name, |(_, suffix)| suffix);
    [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIALS",
    ]
    .iter()
    .any(|word| suffix.eq_ignore_ascii_case(word))
}

/// What this run has masked, as the surface shows it: the name that introduced
/// each value, why it was taken, how much of it is shown, and how often it left.
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

fn rows(mut prints: Vec<Footprint>) -> Listing {
    let total: u64 = prints.iter().map(|print| print.hits).sum();
    let values = prints.len();
    // What left most stands first: that is what a reader came to see.
    prints.sort_by(|a, b| b.hits.cmp(&a.hits).then_with(|| label(a).cmp(label(b))));
    // The report lines up every column but the last; the ends are padded so
    // they read left to right under their heading instead of flush right.
    let shown: Vec<String> = prints.iter().map(ends).collect();
    let shown_w = widest(shown.iter().map(String::as_str)).max("VALUE".len());
    let mut rows = vec![
        Row::new(["Mask"]).with_role(Role::Title),
        Row::new([
            "values".into(),
            values.to_string(),
            "masked".into(),
            format!("{total}× total"),
        ])
        .with_role(Role::Metric),
        Row::new([
            "NAME".into(),
            "BY".into(),
            "CHARS".into(),
            "MASKED".into(),
            pad("VALUE", shown_w),
        ])
        .with_role(Role::Section),
    ];
    rows.extend(prints.iter().zip(shown).map(|(print, ends)| {
        Row::new([
            clip(label(print), NAME_MAX),
            reason(print).to_string(),
            print.len.to_string(),
            print.hits.to_string(),
            pad(&ends, shown_w),
        ])
        .with_role(Role::Budget)
    }));
    Listing::report(rows)
}

// What a listing calls a value: the name that introduced it, or a dash when
// only a `secrets` list holds it.
fn label(print: &Footprint) -> &str {
    print.name.as_deref().unwrap_or("—")
}

// Why the mask took it: the owner's word, or a shape rule's guess.
fn reason(print: &Footprint) -> &'static str {
    match print.tier {
        Tier::Named => "named",
        Tier::Guessed => "shape",
    }
}

// The two ends of a value, marked as cut when there is more of it than shown.
fn ends(print: &Footprint) -> String {
    let shown = print.head.chars().count() + print.tail.chars().count();
    // Both ends only while they stay a minority of the value: two thirds of a
    // short secret is enough to guess the rest of it.
    if shown * 2 <= print.len {
        format!("{}…{}", print.head, print.tail)
    } else {
        print.head.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(body: &str) -> config::Config {
        config::Config::in_force(toml::from_str(body).expect("a tree")).expect("a config")
    }

    // The listing is the only place a person sees what was masked: the name
    // that introduced each value, the two ends of it, marked only when there
    // is more of it than is shown, why it was taken and how often it left.
    #[test]
    fn the_listing_shows_name_ends_why_and_how_often_it_left() {
        let mut mask = Mask::new([7; 32]);
        mask.add(Some("OLD_PASSWORD"), "hunter2-the-password");
        mask.add(Some("provider.up.api_key"), "short-key");
        // Twelve characters: its two ends would be two thirds of it.
        mask.add(None, "abcdefghijkl");
        let _ = mask.cover("hunter2-the-password");

        let prints = mask.footprints();
        let listing = rows(prints.clone());
        // The ends identify a value here; its name need not, since a `secrets`
        // list leaves every one of its values unnamed.
        let row_of = |print: &Footprint| {
            listing
                .rows
                .iter()
                .find(|row| row.role == Role::Budget && row.cells[4].trim_end() == ends(print))
                .expect("a row per value")
        };
        let table: Vec<&Row> = listing
            .rows
            .iter()
            .filter(|row| row.role == Role::Budget)
            .collect();
        assert_eq!(table.len(), prints.len());
        // The column set is the listing's whole shape: a row of another width
        // would put the ends under the wrong heading.
        let header = listing
            .rows
            .iter()
            .find(|row| row.role == Role::Section)
            .expect("a header");
        assert_eq!(header.cells.len(), table[0].cells.len(), "{header:?}");
        for print in &prints {
            let row = row_of(print);
            assert_eq!(
                row.cells[0].trim_end(),
                clip(label(print), NAME_MAX),
                "{row:?}"
            );
            assert!(row.cells[0].trim_end().len() <= NAME_MAX, "{row:?}");
            assert!(!row.cells[0].contains("{{"), "{row:?}");
            assert!(row.cells[4].trim_end().starts_with(&print.head), "{row:?}");
            assert_eq!(row.cells[2].trim(), print.len.to_string(), "{row:?}");
            assert_eq!(row.cells[3].trim(), print.hits.to_string(), "{row:?}");
        }
        // A value only a `secrets` list holds is named by nothing, and says so.
        assert_eq!(row_of(&prints[2]).cells[0].trim_end(), "—");
        let long = prints
            .iter()
            .find(|print| print.len > print.head.chars().count() + print.tail.chars().count())
            .expect("a value longer than its ends");
        let row = row_of(long);
        assert!(row.cells[4].contains('\u{2026}'), "{row:?}");
        // Both ends while the hidden middle is the longer half of the value.
        assert!(row.cells[4].trim_end().ends_with(&long.tail), "{row:?}");
        // A value those two ends would give away shows its head alone.
        let short = prints
            .iter()
            .find(|print| print.len < 2 * (print.head.chars().count() + print.tail.chars().count()))
            .expect("a value shorter than twice its ends");
        let row = row_of(short);
        assert_eq!(row.cells[4].trim_end(), short.head, "{row:?}");

        let summary = listing
            .rows
            .iter()
            .find(|row| row.role == Role::Metric)
            .expect("a summary");
        assert_eq!(summary.cells[1], prints.len().to_string(), "{summary:?}");
        let total: u64 = prints.iter().map(|print| print.hits).sum();
        assert!(
            summary.cells[3].starts_with(&total.to_string()),
            "{summary:?}"
        );
    }

    #[test]
    fn the_shape_rule_exempts_plain_urls() {
        assert!(!shapes_a_secret("https://api.example.com/v1"));
        assert!(!shapes_a_secret("git+ssh://host/repo.git"));
        assert!(!shapes_a_secret("rfc2396://id/hello"));
        assert!(shapes_a_secret("postgres://user:pass@host:5432/db"));
        assert!(shapes_a_secret("git+ssh://git@host/repo.git"));
        assert!(shapes_a_secret("https://host/v1?token=example"));
        assert!(shapes_a_secret("https://host/v1#token=example"));
        assert!(shapes_a_secret("ratio 2:1 in the mix 100%"));
        assert!(shapes_a_secret("://not-a-scheme-0123"));
        assert!(!shapes_a_secret("plain text://"));
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
            values_of([], &config),
            vec![
                Secret {
                    name: Some("provider.down.api_key".to_string()),
                    tier: Tier::Named,
                    value: kept,
                },
                Secret {
                    name: None,
                    tier: Tier::Named,
                    value: "abcd".to_string(),
                },
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
            pi_store::envfile::path(&tools),
            "TELEGRAM_BOT_TOKEN=123456:AAbbCCddEEffGG\nPASSWD=123456\nSHORT_KEY=ab\n\
             MODE=production\nDEBUG=1\n",
        )
        .expect("the env file");
        let config = config(
            "secrets = [\"hunter2-the-password\"]\n\
             [provider.up]\nbase_url = \":3425/v1\"\nformat = \"chat\"\n\
             api_key = \"sk-up-0123456789\"\n",
        );

        let mut mask = Mask::new([7; 32]);
        for secret in values_of([tools], &config) {
            secret.cover_into(&mut mask);
        }
        let text = "key sk-up-0123456789 bot 123456:AAbbCCddEEffGG \
                    pass hunter2-the-password six 123456 mode production debug 1";
        let covered = mask.cover(text);
        assert!(!covered.contains("sk-up-0123456789"), "{covered}");
        assert!(!covered.contains("AAbbCCddEEffGG"), "{covered}");
        assert!(!covered.contains("hunter2-the-password"), "{covered}");
        // Six digits are a password when the name says so, and nothing when
        // it does not.
        assert!(!covered.contains("six 123456"), "{covered}");
        let prints = mask.footprints();
        let named = prints
            .iter()
            .find(|print| print.head.starts_with("1234"))
            .expect("the password's token");
        assert_eq!(named.tier, Tier::Named, "a named variable is not a guess");
        // The listing says which variable the value came from.
        assert!(
            prints
                .iter()
                .any(|print| print.name.as_deref() == Some("TELEGRAM_BOT_TOKEN")),
            "{prints:?}"
        );
        assert!(
            !prints.iter().any(|print| print.head == "ab"),
            "two characters are left alone: {prints:?}"
        );
        assert!(covered.contains("mode production"), "{covered}");
        assert!(covered.contains("debug 1"), "{covered}");
        assert_eq!(mask.restore(&covered), text);
    }
}
