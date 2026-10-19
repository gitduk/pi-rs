//! A different pi installed under the one running, found by watching the
//! path it was started by — the same path a restart runs again.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;

use crate::core::resolve::Stamp;

// Often enough that an install is noticed before the next prompt goes.
const POLL: Duration = Duration::from_secs(10);
// A binary that does not answer `--version` this fast is not one to run.
const ASK: Duration = Duration::from_secs(5);
// A slow network gives up the check rather than hold a task open.
const ASK_RELEASE: Duration = Duration::from_secs(10);

static LAUNCHER: OnceLock<PathBuf> = OnceLock::new();
static NEWER: Mutex<Option<String>> = Mutex::new(None);
static RELEASED: Mutex<Option<String>> = Mutex::new(None);
static CHANGED: LazyLock<Notify> = LazyLock::new(Notify::new);

/// The path this pi was started by, symlink and all: an upgrade that
/// repoints the link leaves the file `current_exe` names untouched.
pub fn launcher() -> Option<&'static Path> {
    LAUNCHER.get().map(PathBuf::as_path)
}

/// The version now installed at [`launcher`], once it is not the binary
/// running: a rebuild that keeps the version number counts too.
pub fn newer() -> Option<String> {
    NEWER.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The latest release, once it is ahead of the binary running.
pub fn released() -> Option<String> {
    RELEASED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Woken whenever [`newer`] or [`released`] changes.
pub fn changed() -> &'static Notify {
    &CHANGED
}

/// Find the launcher and watch what it resolves to. Inside the runtime, once.
pub fn watch() {
    let Some(path) = find() else {
        return;
    };
    let path = LAUNCHER.get_or_init(|| path);
    let Some(mut seen) = identity(path) else {
        return;
    };
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(POLL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            let Some(now) = identity(path) else {
                continue;
            };
            if now == seen {
                continue;
            }
            // A half-written binary answers nothing; asked again next tick.
            let Some(version) = version(path).await else {
                continue;
            };
            seen = now;
            tracing::info!(target: "pi::update", path = %path.display(), version = %version, "installed");
            *NEWER.lock().unwrap_or_else(PoisonError::into_inner) = Some(format!("v{version}"));
            CHANGED.notify_one();
        }
    });
}

/// Ask `repo`, a GitHub repository URL, for its latest release once, in
/// the background. Inside the runtime.
pub fn check(repo: &str) {
    let latest = format!("{}/releases/latest", repo.trim_end_matches('/'));
    tokio::spawn(async move {
        let tag = match latest_tag(&latest).await {
            Ok(tag) => tag,
            Err(why) => {
                tracing::debug!(target: "pi::update", url = %latest, error = %why, "no release");
                return;
            }
        };
        if !ahead(&tag, env!("CARGO_PKG_VERSION")) {
            return;
        }
        tracing::info!(target: "pi::update", tag = %tag, "released");
        *RELEASED.lock().unwrap_or_else(PoisonError::into_inner) = Some(tag);
        CHANGED.notify_one();
    });
}

// GitHub answers `/releases/latest` with a redirect to the tag's page; the
// API would cost a rate-limited call and a JSON body for the same name.
async fn latest_tag(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("pi/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .timeout(ASK_RELEASE)
        .build()?;
    let res = client.head(url).send().await?;
    let to = res
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("{} without a redirect", res.status()))?;
    let (_, tag) = to
        .rsplit_once("/tag/")
        .ok_or_else(|| anyhow::anyhow!("redirected to {to}"))?;
    Ok(tag.to_string())
}

// A tag that is not plain `x.y.z` (a pre-release, say) is never ahead.
fn ahead(tag: &str, running: &str) -> bool {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.trim_start_matches('v')
            .split('.')
            .map(|n| n.parse().ok())
            .collect()
    };
    matches!((parse(tag), parse(running)), (Some(t), Some(r)) if t > r)
}

// argv[0] as the shell found it; `current_exe` only when that cannot be had.
fn find() -> Option<PathBuf> {
    let found = std::env::args_os().next().and_then(|arg0| {
        let arg0 = PathBuf::from(arg0);
        if arg0.components().count() > 1 {
            return std::path::absolute(arg0).ok();
        }
        std::env::split_paths(&std::env::var_os("PATH")?)
            .map(|dir| dir.join(&arg0))
            .find(|p| p.is_file())
    });
    found.or_else(|| std::env::current_exe().ok())
}

// The file the path resolves to, and its inode: a renamed-in binary can
// keep the old one's size and (unpacked from a tar) its mtime.
fn identity(path: &Path) -> Option<(Stamp, u64)> {
    let target = std::fs::canonicalize(path).ok()?;
    let meta = std::fs::metadata(&target).ok()?;
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let inode = 0;
    Some(((target, meta.modified().ok(), meta.len()), inode))
}

async fn version(path: &Path) -> Option<String> {
    let out = tokio::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(ASK, out).await.ok()?.ok()?;
    if !out.status.success() {
        return None;
    }
    let said = String::from_utf8(out.stdout).ok()?;
    said.split_whitespace().last().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::ahead;

    #[test]
    fn only_a_later_plain_version_is_ahead() {
        assert!(ahead("v2.5.0", "2.4.4"));
        assert!(ahead("v2.4.10", "2.4.9"));
        assert!(!ahead("v2.4.4", "2.4.4"));
        assert!(!ahead("v2.4.3", "2.4.4"));
        assert!(!ahead("v2.5.0-rc1", "2.4.4"));
    }
}
