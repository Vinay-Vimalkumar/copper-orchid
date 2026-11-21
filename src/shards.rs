use memmap2::Mmap;
use std::fs::{self, File};
use std::io;
use std::path::Path;

pub(super) struct Shards {
    maps: Vec<Mmap>,
}

impl Shards {
    pub(super) fn open(dir: &Path) -> io::Result<Self> {
        let mut maps = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|ext| ext == "csv") {
                let file = File::open(path)?;
                if file.metadata()?.len() != 0 {
                    // Input files remain immutable on the read-only data mount.
                    maps.push(unsafe { Mmap::map(&file)? });
                }
            }
        }
        Ok(Self { maps })
    }

    pub(super) fn chunks(&self, target_bytes: usize) -> Vec<&[u8]> {
        let mut chunks = Vec::new();
        for map in &self.maps {
            let Some(header) = memchr::memchr(b'\n', map) else {
                continue;
            };
            let mut body = &map[header + 1..];
            while !body.is_empty() {
                let end = if body.len() <= target_bytes {
                    body.len()
                } else {
                    memchr::memchr(b'\n', &body[target_bytes..])
                        .map_or(body.len(), |n| target_bytes + n + 1)
                };
                chunks.push(&body[..end]);
                body = &body[end..];
            }
        }
        chunks
    }
}
