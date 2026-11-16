//! Publish complete copies and retain the move intent until source cleanup ends.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use super::{LOCK, Location, Store, TRANSCRIPT, bucket_locations, claim_dir, file};

const INTENT: &str = ".move.json";
const INTENT_TEMP: &str = ".move.json.tmp";
const COPY: &str = "session.jsonl.move";

#[derive(Serialize, Deserialize)]
struct Move {
    source: String,
    // Root first; publish and remove it last.
    family: Vec<String>,
}

impl Store {
    pub(super) fn resume_move(&self, workspace: &Path, id: &str) -> Result<bool> {
        let marker = self.session_dir(workspace, id).join(INTENT);
        let bytes = match fs::read(marker) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let intent: Move = serde_json::from_slice(&bytes).context("invalid session move intent")?;
        ensure!(
            intent.family.first().is_some_and(|root| root == id),
            "invalid session move root"
        );
        let unique: std::collections::HashSet<_> = intent.family.iter().collect();
        ensure!(
            unique.len() == intent.family.len(),
            "duplicate session move member"
        );
        self.move_sessions(workspace, id, &intent, false)?;
        Ok(true)
    }

    pub(super) fn move_family(&self, source: &Location, workspace: &Path) -> Result<()> {
        let destination = self.path_of(workspace, source.session.as_str());
        let _source = if source != &destination {
            Some(claim_dir(source.artifacts(), source.session.as_str())?)
        } else {
            None
        };
        ensure!(
            !source.artifacts().join(INTENT).exists(),
            "session has an unfinished move at {}; complete it before moving again",
            source.artifacts().display()
        );
        let bucket = source
            .artifacts()
            .parent()
            .context("session bucket")?
            .to_owned();
        let (state, _) = file::tail(source)?.context("session is not recorded")?;
        let metadata = state.metadata.context("missing session metadata")?;
        let mut family = vec![source.session.as_str().to_owned()];
        for at in bucket_locations(&bucket) {
            if at != *source
                && file::tail(&at)?.is_some_and(|(state, _)| {
                    state.metadata.is_some_and(|metadata| {
                        metadata.parent.as_deref() == Some(source.session.as_str())
                    })
                })
            {
                family.push(at.session.as_str().to_owned());
            }
        }
        // A bucket name can alias another workspace spelling. In that case the
        // file is already here; only its recorded workspace needs changing.
        let origin = self.path_of(Path::new(&metadata.workspace), source.session.as_str());
        ensure!(
            origin.path == source.path,
            "session source workspace does not match its path"
        );
        self.move_sessions(
            workspace,
            source.session.as_str(),
            &Move {
                source: metadata.workspace,
                family,
            },
            true,
        )
    }

    fn move_sessions(
        &self,
        workspace: &Path,
        root: &str,
        intent: &Move,
        starting: bool,
    ) -> Result<()> {
        let source_workspace = Path::new(&intent.source);
        let destination_root = self.session_dir(workspace, root);
        let mut claims = Vec::new();
        let mut family = Vec::new();
        for id in &intent.family {
            let source = self.path_of(source_workspace, id);
            let destination = self.path_of(workspace, id);
            if source != destination {
                if (id != root || !starting) && source.artifacts().exists() {
                    claims.push(claim_dir(source.artifacts(), id)?);
                }
                if starting {
                    ensure!(
                        !destination.path.exists(),
                        "destination session already exists"
                    );
                }
            }
            if id != root {
                claims.push(self.claim(workspace, id)?);
            }
            family.push((source, destination));
        }
        let marker = destination_root.join(INTENT);
        if starting {
            let temp = destination_root.join(INTENT_TEMP);
            let mut options = fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temp)?;
            file.write_all(&serde_json::to_vec(intent)?)?;
            file.sync_all()?;
            fs::rename(temp, &marker)?;
        }
        // A previous attempt may have returned an error after renaming the
        // marker but before syncing it. Retry its durability before copying.
        fs::File::open(&marker)?.sync_all()?;
        file::sync_dir(&destination_root)?;
        if let Some(bucket) = destination_root.parent() {
            file::sync_dir(bucket)?;
        }
        file::sync_dir(self.root())?;

        for (source, destination) in family.iter().rev() {
            if source.path.exists() {
                let temp = destination.artifacts().join(COPY);
                fs::copy(&source.path, &temp)?;
                let staged = Location::new(temp, source.session.clone());
                // This also syncs the complete copy, truncating any torn tail.
                file::relocate(&staged, workspace)?;
                let mut index = destination.cached_index();
                fs::rename(&staged.path, &destination.path)?;
                *index = file::Index::new(destination.session.clone());
            } else {
                // Cleanup may already have removed a source in a prior attempt.
                let (state, _) = file::tail(destination)?
                    .context("session move lost its source and destination")?;
                ensure!(
                    state
                        .metadata
                        .is_some_and(|metadata| metadata.workspace == workspace.to_string_lossy()),
                    "session move destination is incomplete"
                );
                fs::File::open(&destination.path)?.sync_all()?;
            }
            file::sync_dir(&destination.artifacts())?;
        }
        for (source, destination) in family.iter().rev() {
            if source == destination || !source.artifacts().exists() {
                continue;
            }
            for entry in fs::read_dir(source.artifacts())? {
                let entry = entry?;
                if [LOCK, file::WRITE_LOCK, TRANSCRIPT]
                    .iter()
                    .any(|name| entry.file_name() == *name)
                {
                    continue;
                }
                fs::rename(
                    entry.path(),
                    destination.artifacts().join(entry.file_name()),
                )?;
            }
            file::sync_dir(&destination.artifacts())?;
            // Keep source directories through any fallible artifact moves.
            // A retry can therefore still obtain their process claims.
        }
        for (source, destination) in family.iter().rev() {
            if source != destination && source.artifacts().exists() {
                fs::remove_dir_all(source.artifacts())?;
                file::sync_dir(source.artifacts().parent().context("session bucket")?)?;
            }
        }
        fs::remove_file(marker)?;
        file::sync_dir(&destination_root)?;
        Ok(())
    }
}
