//! A clipboard image, saved where `read` can open it: a terminal pastes text
//! only, so the image is asked of the system clipboard instead.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const TYPES: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

pub(super) fn paste_image() -> Result<PathBuf, String> {
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    if !wayland && std::env::var_os("DISPLAY").is_none() {
        return Err("no display here, so no clipboard to paste from".to_string());
    }
    let (program, list, get): (&str, &[&str], &[&str]) = if wayland {
        ("wl-paste", &["--list-types"], &["--no-newline", "--type"])
    } else {
        (
            "xclip",
            &["-selection", "clipboard", "-o", "-t", "TARGETS"],
            &["-selection", "clipboard", "-o", "-t"],
        )
    };
    let run = |args: &[&str]| -> Result<Vec<u8>, String> {
        let out = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "pasting an image needs wl-paste, or xclip on X11".to_string()
                }
                _ => format!("{program}: {e}"),
            })?;
        match out.status.success() {
            true => Ok(out.stdout),
            false => Err("no image on the clipboard".to_string()),
        }
    };

    let offered = String::from_utf8_lossy(&run(list)?).into_owned();
    let (media_type, ext) = TYPES
        .iter()
        .find(|(t, _)| offered.lines().any(|l| l.trim() == *t))
        .ok_or("no image on the clipboard")?;
    let bytes = run(&[get, &[media_type]].concat())?;
    if bytes.is_empty() {
        return Err("no image on the clipboard".to_string());
    }

    let dir = pi_store::dir()
        .ok_or("no pi home to save the image in")?
        .join("images");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let path = dir.join(format!("{stamp}.{ext}"));
    std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(path)
}
