use super::{fields, shards::Shards};
use ahash::AHashMap;
use compact_str::CompactString;
use rayon::prelude::*;
use std::collections::hash_map::Entry;
use std::io;
use std::path::Path;

#[derive(Default)]
pub(super) struct NameIndex {
    short: AHashMap<u64, u32>,
    other: AHashMap<Box<str>, u32>,
}

// For IDs beginning with 'u', pack up to eight suffix bytes into one word.
// A trailing NUL uses the string table so different lengths cannot alias.
#[inline]
fn packed(name: &str) -> Option<u64> {
    let [b'u', suffix @ ..] = name.as_bytes() else {
        return None;
    };
    match suffix {
        [a, b, c, d, e, f, g, h] if *h != 0 => {
            Some(u64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, *h]))
        }
        [a, b, c, d, e, f, g] if *g != 0 => {
            Some(u64::from_be_bytes([*a, *b, *c, *d, *e, *f, *g, 0]))
        }
        _ if suffix.len() < 7 && suffix.last() != Some(&0) => {
            let mut bytes = [0; 8];
            bytes[..suffix.len()].copy_from_slice(suffix);
            Some(u64::from_be_bytes(bytes))
        }
        _ => None,
    }
}

impl NameIndex {
    #[inline]
    pub(super) fn get(&self, name: &str) -> Option<&u32> {
        match packed(name) {
            Some(key) => self.short.get(&key),
            None => self.other.get(name),
        }
    }

    pub(super) fn values_mut(&mut self) -> impl Iterator<Item = &mut u32> {
        self.short.values_mut().chain(self.other.values_mut())
    }
}

#[derive(Default)]
pub(super) struct Names {
    pub(super) index: NameIndex,
    pub(super) names: Vec<compact_str::CompactString>,
}

impl Names {
    pub(super) fn id(&mut self, name: &str) -> u32 {
        if let Some(key) = packed(name) {
            match self.index.short.entry(key) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let id = u32::try_from(self.names.len()).expect("too many distinct names");
                    self.names.push(name.into());
                    entry.insert(id);
                    id
                }
            }
        } else if let Some(&id) = self.index.other.get(name) {
            id
        } else {
            let id = u32::try_from(self.names.len()).expect("too many distinct names");
            self.names.push(name.into());
            self.index.other.insert(name.into(), id);
            id
        }
    }
}

// The first byte of a valid UTF-8 suffix cannot be 0xff. Use that otherwise
// unreachable prefix to refer to the exact spelling of a non-inline name.
const INDIRECT: u64 = 0xff00_0000_0000_0000;

#[derive(Clone, Copy)]
struct Label(u64);

impl Label {
    fn text(self, spellings: &[&str]) -> CompactString {
        if self.0 >= INDIRECT {
            return spellings[(self.0 - INDIRECT) as usize].into();
        }
        let mut bytes = [b'u'; 9];
        bytes[1..].copy_from_slice(&self.0.to_be_bytes());
        let len = 9 - self.0.trailing_zeros() as usize / 8;
        std::str::from_utf8(&bytes[..len])
            .expect("packed UTF-8")
            .into()
    }

    fn min(self, other: Label, spellings: &[&str]) -> Label {
        if self.0 < INDIRECT && other.0 < INDIRECT {
            return Label(self.0.min(other.0));
        }
        if self.text(spellings) <= other.text(spellings) {
            self
        } else {
            other
        }
    }
}

struct Component {
    parent: u32,
    size: u32,
    smallest: Label,
}

struct Links<'a> {
    index: NameIndex,
    sets: Vec<Component>,
    spellings: Vec<&'a str>,
}

impl<'a> Links<'a> {
    fn new(capacity: usize) -> Self {
        Self {
            index: NameIndex {
                short: AHashMap::with_capacity(capacity),
                other: AHashMap::new(),
            },
            sets: Vec::with_capacity(capacity),
            spellings: Vec::new(),
        }
    }

    fn node(nodes: &mut Vec<Component>, label: Label) -> u32 {
        let id = u32::try_from(nodes.len()).expect("too many distinct names");
        nodes.push(Component {
            parent: id,
            size: 1,
            smallest: label,
        });
        id
    }

    fn intern_short(&mut self, key: u64) -> u32 {
        *self
            .index
            .short
            .entry(key)
            .or_insert_with(|| Self::node(&mut self.sets, Label(key)))
    }

    fn intern(&mut self, name: &'a str) -> u32 {
        if let Some(key) = packed(name) {
            return self.intern_short(key);
        }
        if let Some(&id) = self.index.other.get(name) {
            return id;
        }
        let label = Label(INDIRECT + self.spellings.len() as u64);
        self.spellings.push(name);
        let id = Self::node(&mut self.sets, label);
        self.index.other.insert(name.into(), id);
        id
    }

    fn join(&mut self, a: u32, b: u32) {
        let mut a = find(&mut self.sets, a);
        let mut b = find(&mut self.sets, b);
        if a == b {
            return;
        }
        if self.sets[a].size < self.sets[b].size {
            std::mem::swap(&mut a, &mut b);
        }
        self.sets[b].parent = a as u32;
        self.sets[a].size += self.sets[b].size;
        self.sets[a].smallest = self.sets[a]
            .smallest
            .min(self.sets[b].smallest, &self.spellings);
    }

    fn names(mut self) -> Names {
        let mut names = Vec::new();
        for (i, component) in self.sets.iter_mut().enumerate() {
            if component.parent as usize == i {
                let id = names.len();
                names.push(component.smallest.text(&self.spellings));
                // The representative has been materialized once. Reuse this
                // slot for its dense person ID when resolving aliases below.
                component.smallest = Label(id as u64);
            }
        }
        for id in self.index.values_mut() {
            let root = find(&mut self.sets, *id);
            *id = self.sets[root].smallest.0 as u32;
        }
        Names {
            names,
            index: self.index,
        }
    }
}

fn find(nodes: &mut [Component], mut id: u32) -> usize {
    while nodes[id as usize].parent != id {
        let parent = nodes[id as usize].parent as usize;
        let next = nodes[parent].parent;
        nodes[id as usize].parent = next;
        id = next;
    }
    id as usize
}

pub(super) fn linked_users(dir: &Path) -> io::Result<Names> {
    let shards = Shards::open(dir)?;
    let chunks = shards.chunks(256 * 1024);
    type Batch<'a> = (Vec<[u64; 2]>, Vec<[&'a str; 2]>);
    fn parse(chunk: &[u8]) -> io::Result<Batch<'_>> {
        let text = std::str::from_utf8(chunk)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut short = Vec::with_capacity(chunk.len() / 24);
        let mut other = Vec::new();
        for [a, b] in text.split('\n').filter_map(fields::<2>) {
            if a.is_empty() || b.is_empty() {
                continue;
            }
            match (packed(a), packed(b)) {
                (Some(a), Some(b)) => short.push([a, b]),
                _ => other.push([a, b]),
            }
        }
        Ok((short, other))
    }
    let size: usize = chunks.iter().map(|chunk| chunk.len()).sum();
    let batches = if size < 2 * 1024 * 1024 {
        chunks
            .into_iter()
            .map(parse)
            .collect::<io::Result<Vec<_>>>()?
    } else {
        chunks
            .into_par_iter()
            .map(parse)
            .collect::<io::Result<Vec<_>>>()?
    };
    let edges = batches
        .iter()
        .map(|(short, other)| short.len() + other.len())
        .sum::<usize>();
    let mut links = Links::new((edges + edges / 2).min(4_000_000));
    for (short, other) in batches {
        for [a, b] in short {
            let a = links.intern_short(a);
            let b = links.intern_short(b);
            links.join(a, b);
        }
        for [a, b] in other {
            let a = links.intern(a);
            let b = links.intern(b);
            links.join(a, b);
        }
    }
    Ok(links.names())
}
