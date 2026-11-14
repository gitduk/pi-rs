//! Read-only import of legacy request segments and their committed index.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use message::{MessageId, SessionId, WireMessage};
use serde::{Deserialize, Serialize};

pub const FORMAT: u8 = 1;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub format: u8,
    pub session: SessionId,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub message: WireMessage,
    pub parent: Option<MessageId>,
    pub native_call: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Index {
    format: u8,
    session: SessionId,
    segments: Vec<Segment>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Segment {
    from: usize,
    to: usize,
}

impl Segment {
    fn path(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}-{}.json", self.from, self.to))
    }
}

fn index(dir: &Path) -> Result<Option<Index>> {
    let path = dir.join("index.json");
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot read {}", path.display())),
    };
    let index: Index = serde_json::from_reader(std::io::BufReader::new(file))
        .with_context(|| format!("invalid message index {}", path.display()))?;
    ensure!(
        index.format == FORMAT,
        "unsupported message archive format {}",
        index.format
    );
    let mut next = 0;
    for segment in &index.segments {
        ensure!(
            segment.from == next && segment.to > segment.from,
            "invalid message segment range"
        );
        next = segment.to;
    }
    Ok(Some(index))
}

fn visit_segment(
    dir: &Path,
    segment: &Segment,
    mut visit: impl FnMut(usize, Entry) -> Result<()>,
) -> Result<()> {
    struct Entries<'a, F> {
        from: usize,
        to: usize,
        visit: &'a mut F,
    }
    impl<'de, F: FnMut(usize, Entry) -> Result<()>> serde::de::Visitor<'de> for Entries<'_, F> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a message segment")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<(), A::Error> {
            let mut at = self.from;
            while let Some(entry) = sequence.next_element()? {
                if at >= self.to {
                    return Err(serde::de::Error::custom("message segment is too long"));
                }
                (self.visit)(at, entry).map_err(serde::de::Error::custom)?;
                at += 1;
            }
            if at != self.to {
                return Err(serde::de::Error::custom("message segment is too short"));
            }
            Ok(())
        }
    }
    let path = segment.path(dir);
    let file =
        std::fs::File::open(&path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut decoder = serde_json::Deserializer::from_reader(std::io::BufReader::new(file));
    serde::de::Deserializer::deserialize_seq(
        &mut decoder,
        Entries {
            from: segment.from,
            to: segment.to,
            visit: &mut visit,
        },
    )?;
    decoder.end()?;
    Ok(())
}

pub fn read(dir: &Path) -> Result<Option<Archive>> {
    let Some(index) = index(dir)? else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    for segment in index.segments {
        visit_segment(dir, &segment, |_, entry| {
            entries.push(entry);
            Ok(())
        })?;
    }
    Ok(Some(Archive {
        format: index.format,
        session: index.session,
        entries,
    }))
}
