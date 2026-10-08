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

    let dir = pi_store::images_dir().ok_or("no pi home to save the image in")?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // Named by content, so pasting the same picture again is the same file.
    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut h);
        h.finish()
    };
    let path = dir.join(format!("{hash:016x}.{ext}"));
    // Already there, it is touched instead: a fresh paste keeps it from the sweep.
    let kept = std::fs::File::options()
        .write(true)
        .open(&path)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()));
    if kept.is_err() {
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
/// the model, and those files once each in order; `None` when it names none.
pub(super) fn with_paths(text: &str, images: &[PathBuf]) -> Option<(String, Vec<PathBuf>)> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut named: Vec<PathBuf> = Vec::new();
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
                if !named.contains(path) {
                    named.push(path.clone());
                }
            }
            None => out.push_str(&tag[..=close]),
        }
        rest = &tag[close + 1..];
    }
    out.push_str(rest);
    (!named.is_empty()).then_some((out, named))
}

/// Each file as an image to send with the ask. One that will not go is left
/// to its path in the text, where `read` says why.
pub(super) fn attached(paths: &[PathBuf]) -> Vec<llm::message::Image> {
    paths
        .iter()
        .filter_map(|p| toolbox::read::as_image(&std::fs::read(p).ok()?).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pasted_images_tag_gains_its_path_and_nothing_else_moves() {
        let images = [PathBuf::from("/i/1.png")];
        let (sent, named) = with_paths(
            "see [Image #1 8x8 75 B] and [Image #2 x] [Image #1 8x8 75 B] [Image #1",
            &images,
        )
        .unwrap();
        assert_eq!(
            sent,
            "see [Image #1 8x8 75 B: /i/1.png] and [Image #2 x] \
             [Image #1 8x8 75 B: /i/1.png] [Image #1"
        );
        assert_eq!(named, images, "named twice, sent once");
        assert_eq!(with_paths("no image here", &images), None);
    }

    #[test]
    fn a_named_file_goes_as_an_image_and_one_that_is_not_stays_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend([0; 16]);
        std::fs::write(&png, bytes).unwrap();
        let text = dir.path().join("b.png");
        std::fs::write(&text, "not an image").unwrap();
        let gone = dir.path().join("c.png");
        let sent = attached(&[png, text, gone]);
        assert_eq!(sent.len(), 1);
        assert!(
            matches!(&sent[0], llm::message::Image::Base64 { media_type, .. } if media_type == "image/png")
        );
    }
}
