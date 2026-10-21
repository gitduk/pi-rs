//! What says a file or some bytes changed since last time, and a value built
//! from files that is built again only when that says so.

use std::fs::Metadata;
use std::hash::Hasher as _;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

/// A file as last seen: its path, time and size.
pub type Stamp = (PathBuf, Option<SystemTime>, u64);

/// `path` as it stands now; `None` when it cannot be looked at.
pub fn of(path: PathBuf) -> Option<Stamp> {
    let meta = std::fs::metadata(&path).ok()?;
    Some(from_meta(path, &meta))
}

/// The stamp of `path` from metadata already in hand.
pub fn from_meta(path: PathBuf, meta: &Metadata) -> Stamp {
    (path, meta.modified().ok(), meta.len())
}

/// FNV-1a over bytes: a cheap answer to "the same bytes as last time?" that
/// holds still across builds, never a defence against bytes chosen to collide.
#[derive(Clone, Copy)]
pub struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl std::hash::Hasher for Fnv {
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= u64::from(*b);
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }

    fn finish(&self) -> u64 {
        self.0
    }
}

/// [`Fnv`] of `bytes` in one go.
pub fn fnv(bytes: &[u8]) -> u64 {
    let mut h = Fnv::default();
    h.write(bytes);
    h.finish()
}

/// A value built from files, kept until their stamps move.
pub struct Stamped<T> {
    seen: Mutex<Option<(Vec<Stamp>, T)>>,
}

impl<T> Default for Stamped<T> {
    fn default() -> Self {
        Self {
            seen: Mutex::new(None),
        }
    }
}

impl<T> Stamped<T> {
    /// Read the value as the files stand at `now`, built by `build` on the
    /// first read and whenever `now` differs from the stamps it was built at.
    pub fn read<R>(
        &self,
        now: Vec<Stamp>,
        build: impl FnOnce() -> T,
        read: impl FnOnce(&T) -> R,
    ) -> R {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let kept = seen.take().filter(|(at, _)| *at == now);
        let (_, value) = seen.insert(kept.unwrap_or_else(|| (now, build())));
        read(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Journal fingerprints are compared across builds, so the hash is FNV-1a
    // exactly, pinned by the reference vectors.
    #[test]
    fn fnv_is_fnv_1a_64() {
        assert_eq!(fnv(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv(b"foobar"), 0x8594_4171_f739_67e8);
    }

    // A first read builds even when no file is there, and only a moved
    // stamp builds again: an empty directory still owes its built-ins.
    #[test]
    fn builds_first_then_only_on_a_moved_stamp() {
        let cache = Stamped::default();
        let builds = std::cell::Cell::new(0);
        let build = || {
            builds.set(builds.get() + 1);
            builds.get()
        };
        assert_eq!(cache.read(Vec::new(), build, |v| *v), 1);
        assert_eq!(cache.read(Vec::new(), build, |v| *v), 1);
        let moved = vec![(PathBuf::from("a"), None, 1)];
        assert_eq!(cache.read(moved.clone(), build, |v| *v), 2);
        assert_eq!(cache.read(moved, build, |v| *v), 2);
    }
}
