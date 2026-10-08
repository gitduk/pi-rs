//! A clipboard image, saved where `read` can open it: a terminal pastes text
//! only, so the image is asked of the system clipboard instead.

use std::path::PathBuf;

use super::editor::IMAGE_TAG;
use std::process::{Command, Stdio};

const TYPES: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/gif", "gif"),
    ("image/webp", "webp"),
];

/// The saved file, and what the line calls it: `1280x720 245.3 KB`.
pub(super) fn paste_image() -> Result<(PathBuf, String), String> {
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
    // Named by content, so pasting the same picture again is the same file.
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        h.finish()
    };
    let path = dir.join(format!("{hash:016x}.{ext}"));
    if !path.exists() {
        std::fs::write(&path, &bytes).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let size = pi_store::text::size(bytes.len() as u64);
    let about = match imagesize::blob_size(&bytes) {
        Ok(d) => format!("{}x{} {size}", d.width, d.height),
        Err(_) => size,
    };
    Ok((path, about))
}

/// `text` with each `[Image #n …]` naming a pasted file given its path, for
/// the model; `None` when it names none.
pub(super) fn with_paths(text: &str, images: &[PathBuf]) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut found = false;
    while let Some(at) = rest.find(IMAGE_TAG) {
        let (before, tag) = rest.split_at(at);
        out.push_str(before);
        let Some(close) = tag.find(']') else {
            rest = tag;
            break;
        };
        let n: Option<usize> = tag[IMAGE_TAG.len()..close]
            .split(' ')
            .next()
            .and_then(|n| n.parse().ok());
        match n.and_then(|n| images.get(n.checked_sub(1)?)) {
            Some(path) => {
                out.push_str(&format!("{}: {}]", &tag[..close], path.display()));
                found = true;
            }
            None => out.push_str(&tag[..=close]),
        }
        rest = &tag[close + 1..];
    }
    out.push_str(rest);
    found.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_images_tag_gains_its_path_and_nothing_else_moves() {
        let images = [PathBuf::from("/i/1.png")];
        let sent = with_paths(
            "see [Image #1 8x8 75 B] and [Image #2 x] [Image #1",
            &images,
        );
        assert_eq!(
            sent.as_deref(),
            Some("see [Image #1 8x8 75 B: /i/1.png] and [Image #2 x] [Image #1")
        );
        assert_eq!(with_paths("no image here", &images), None);
    }
}
