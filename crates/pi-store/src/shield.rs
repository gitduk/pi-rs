//! The key that names what must not leave the machine.
//!
//! A token reads the same in every run only because this key does: it is made
//! once, kept beside the rest of pi's state, and read back from then on.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

const FILE: &str = "shield.key";
const BYTES: usize = 32;

// A key this process minted, kept even when it could not be written: a second
// call must not mint another, or tokens already sent would stop meaning it.
static MINTED: Mutex<Option<[u8; BYTES]>> = Mutex::new(None);

/// This machine's masking key, made on first use with `0600`. `None` when
/// there is no pi directory to keep it in.
pub fn key() -> Option<[u8; BYTES]> {
    let path = crate::dir()?.join(FILE);
    if let Some(key) = read(&path) {
        return Some(key);
    }
    let mut minted = MINTED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(key) = *minted {
        return Some(key);
    }
    let key = fresh()?;
    // Another pi may have written it between the read and here.
    let key = write(&path, key).or_else(|| read(&path))?;
    *minted = Some(key);
    Some(key)
}

fn read(path: &Path) -> Option<[u8; BYTES]> {
    std::fs::read(path).ok()?.try_into().ok()
}

// Made with the mode it is meant to keep, rather than chmodded after a window
// where it is readable by anyone.
fn write(path: &Path, key: [u8; BYTES]) -> Option<[u8; BYTES]> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).ok()?;
    std::io::Write::write_all(&mut file, &key).ok()?;
    Some(key)
}

// A guessable key would be worse than none: it is the only thing keeping a
// token from being checkable against a guess off the machine.
fn fresh() -> Option<[u8; BYTES]> {
    use std::io::Read as _;
    let mut key = [0u8; BYTES];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut key)
        .ok()?;
    Some(key)
}
