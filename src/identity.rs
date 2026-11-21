use ahash::AHashMap;
use std::collections::hash_map::Entry;

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
            Some(u64::from_le_bytes([*a, *b, *c, *d, *e, *f, *g, *h]))
        }
        [a, b, c, d, e, f, g] if *g != 0 => {
            Some(u64::from_le_bytes([*a, *b, *c, *d, *e, *f, *g, 0]))
        }
        _ if suffix.len() < 7 && suffix.last() != Some(&0) => {
            let mut bytes = [0; 8];
            bytes[..suffix.len()].copy_from_slice(suffix);
            Some(u64::from_le_bytes(bytes))
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
    pub(super) names: Vec<String>,
}

impl Names {
    pub(super) fn id(&mut self, name: &str) -> u32 {
        if let Some(key) = packed(name) {
            match self.index.short.entry(key) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let id = u32::try_from(self.names.len()).expect("too many distinct names");
                    self.names.push(name.to_owned());
                    entry.insert(id);
                    id
                }
            }
        } else if let Some(&id) = self.index.other.get(name) {
            id
        } else {
            let id = u32::try_from(self.names.len()).expect("too many distinct names");
            self.names.push(name.to_owned());
            self.index.other.insert(name.into(), id);
            id
        }
    }
}
