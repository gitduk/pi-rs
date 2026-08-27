//! One-shot migration for transcripts that still nest the session under a
//! `log` key. Rewrites each file so `entries` and `next` sit at the top level,
//! the shape the store now writes. Idempotent: already-flat files are skipped,
//! and the original file mode (0600) is kept.
//!
//! Usage: `pi-migrate-sessions [DIR]`, defaulting to `~/.local/state/pi/sessions`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

fn main() -> Result<()> {
    let dir = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            tools::state::dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("sessions")
        });

    let mut rewritten = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
    {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match flatten(&path) {
            Ok(true) => rewritten += 1,
            Ok(false) => {}
            Err(e) => failed.push(format!("{}: {e:#}", path.display())),
        }
    }

    println!("{rewritten} transcript(s) rewritten");
    if failed.is_empty() {
        return Ok(());
    }
    for line in &failed {
        eprintln!("failed: {line}");
    }
    bail!("{} could not be migrated", failed.len());
}

/// Lift a nested `log` object to the top level. False when already flat.
fn flatten(path: &Path) -> Result<bool> {
    let body = fs::read_to_string(path)?;
    let mut value: serde_json::Value = serde_json::from_str(&body)?;
    let Some(obj) = value.as_object_mut() else {
        return Ok(false);
    };
    let Some(nested) = obj.remove("log") else {
        return Ok(false);
    };
    let Some(flat) = nested.as_object() else {
        bail!("`log` is not an object");
    };
    for (k, v) in flat {
        obj.insert(k.clone(), v.clone());
    }

    #[cfg(unix)]
    let mode = fs::metadata(path)?.permissions().mode();
    fs::write(path, serde_json::to_vec_pretty(&value)?)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(true)
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

