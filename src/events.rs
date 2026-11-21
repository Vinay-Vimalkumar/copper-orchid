use super::{DAY, Names, days_before_year, decimal, fields};
use crate::model::Event;
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fs::{self, File};
use std::io;
use std::path::Path;

const CHUNK: usize = 2 << 20;
const INVALID: i64 = i64::MIN;

#[inline]
fn pair(b: &[u8], i: usize) -> Option<i64> {
    let a = b[i].wrapping_sub(b'0');
    let b = b[i + 1].wrapping_sub(b'0');
    (a < 10 && b < 10).then(|| i64::from(a) * 10 + i64::from(b))
}

// The input window spans few months. Cache the verified date and check the entire
// year-month prefix on every hit, including both hyphens.
#[derive(Default)]
struct Calendar {
    days: [Option<(u64, i64)>; 32],
}

impl Calendar {
    #[inline]
    fn parse(&mut self, text: &str) -> Option<i64> {
        let b = text.as_bytes();
        if !matches!(b.len(), 20 | 25) || b[4] != b'-' {
            return decimal(b);
        }
        if b[10] != b'T' || b[13] != b':' || b[16] != b':' {
            return None;
        }
        let day = pair(b, 8)? as usize;
        if !(1..32).contains(&day) {
            return None;
        }
        let prefix = u64::from_ne_bytes(b[..8].try_into().unwrap());
        let midnight = match self.days[day] {
            Some((old, ms)) if prefix == old => ms,
            _ => {
                if b[7] != b'-' {
                    return None;
                }
                let year = pair(b, 0)? * 100 + pair(b, 2)?;
                let month = pair(b, 5)? as usize;
                if year == 0 || !(1..=12).contains(&month) {
                    return None;
                }
                let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
                let lengths = [
                    31,
                    28 + usize::from(leap),
                    31,
                    30,
                    31,
                    30,
                    31,
                    31,
                    30,
                    31,
                    30,
                    31,
                ];
                if day > lengths[month - 1] {
                    return None;
                }
                let days = days_before_year(year) - days_before_year(1970)
                    + lengths[..month - 1].iter().sum::<usize>() as i64
                    + day as i64
                    - 1;
                let ms = days * DAY;
                self.days[day] = Some((prefix, ms));
                ms
            }
        };
        let hour = pair(b, 11)?;
        let minute = pair(b, 14)?;
        let second = pair(b, 17)?;
        if hour >= 24 || minute >= 60 || second >= 60 {
            return None;
        }
        let mut seconds = hour * 3600 + minute * 60 + second;
        match &b[19..] {
            b"Z" => {}
            [sign @ (b'+' | b'-'), _, _, b':', _, _] => {
                let hour = pair(b, 20)?;
                let minute = pair(b, 23)?;
                if hour >= 24 || minute >= 60 {
                    return None;
                }
                let offset = hour * 3600 + minute * 60;
                seconds += if *sign == b'+' { -offset } else { offset };
            }
            _ => return None,
        }
        Some(midnight + seconds * 1000)
    }
}

// The marker 'e' and zero padding are implicit. Requiring a nonzero trailing
// byte makes this encoding injective. Every other nonempty key takes the text path.
#[inline]
fn inline_id(id: &str) -> Option<u64> {
    let b = id.as_bytes();
    if b.first() != Some(&b'e') || b.len() > 9 || b.last() == Some(&0) {
        return None;
    }
    let suffix = &b[1..];
    let bytes = if let Ok(full) = suffix.try_into() {
        full
    } else {
        let mut small = [0; 8];
        small[..suffix.len()].copy_from_slice(suffix);
        small
    };
    Some(u64::from_le_bytes(bytes))
}

#[inline]
fn mix(x: u64) -> usize {
    let x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    let x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    (x ^ (x >> 31)) as usize
}

struct Candidate {
    ingest: i64,
    event: Event,
}

#[derive(Default)]
struct Bucket<'a> {
    packed: Vec<(u64, Candidate)>,
    text: Vec<(&'a str, Candidate)>,
}

struct Batch<'a> {
    buckets: Vec<Bucket<'a>>,
    unresolved: Vec<&'a str>,
}

struct Catalog<'a> {
    start: i64,
    end: i64,
    users: &'a Names,
    products: &'a HashMap<&'a str, u32>,
}

fn parse_chunk<'a>(chunk: &'a str, partitions: usize, catalog: &Catalog) -> Batch<'a> {
    let mut calendar = Calendar::default();
    let mut missing = HashMap::<&str, u32>::new();
    let mut unresolved = Vec::new();
    let mut buckets: Vec<_> = (0..partitions)
        .map(|_| Bucket {
            packed: Vec::with_capacity(chunk.len() / 72 / partitions),
            text: Vec::new(),
        })
        .collect();
    let mask = partitions - 1;
    for line in chunk.split('\n') {
        let Some([id, user, raw_ts, ingest, kind, product, raw_price]) = fields(line) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let ingest = calendar.parse(ingest).expect("ingest_ts must be valid F1");
        let ts = calendar
            .parse(raw_ts)
            .filter(|&ts| catalog.start <= ts && ts < catalog.end && ingest - ts <= DAY);
        let price = match kind {
            "view" | "cart" => Some(0),
            "purchase" => decimal(raw_price.as_bytes()).filter(|v| (1..=10_000_000).contains(v)),
            _ => None,
        };
        let event = match (ts, price) {
            (Some(ts), Some(price)) if !user.is_empty() && !product.is_empty() => {
                let user = catalog.users.index.get(user).copied().unwrap_or_else(|| {
                    *missing.entry(user).or_insert_with(|| {
                        let id = u32::try_from(catalog.users.names.len() + unresolved.len())
                            .expect("too many distinct names");
                        unresolved.push(user);
                        id
                    })
                });
                let category = if price == 0 {
                    0
                } else {
                    catalog.products.get(product).copied().unwrap_or(0)
                };
                Event {
                    user,
                    ts,
                    category,
                    price: price as u32,
                }
            }
            // Invalid earliest copies must still suppress later valid copies.
            _ => Event {
                user: 0,
                ts: INVALID,
                category: 0,
                price: 0,
            },
        };
        let item = Candidate { ingest, event };
        if let Some(key) = inline_id(id) {
            buckets[mix(key) & mask].packed.push((key, item));
        } else {
            let hash = id
                .as_bytes()
                .iter()
                .fold(0u64, |acc, &c| acc.wrapping_mul(257).wrapping_add(c.into()));
            buckets[mix(hash) & mask].text.push((id, item));
        }
    }
    Batch {
        buckets,
        unresolved,
    }
}

fn dedup(parts: Vec<(usize, Bucket<'_>)>, base: u32, translations: &[Vec<u32>]) -> Vec<Event> {
    let count = parts.iter().map(|(_, b)| b.packed.len()).sum::<usize>();
    let capacity = (count + count / 2 + 1).next_power_of_two();
    let mut index = vec![usize::MAX; capacity];
    let mut winners: Vec<(u64, Candidate)> = Vec::with_capacity(count);
    let mut others = HashMap::<&str, Candidate>::new();

    for (chunk, bucket) in parts {
        let canonical = |candidate: &mut Candidate| {
            if candidate.event.ts != INVALID && candidate.event.user >= base {
                candidate.event.user = translations[chunk][(candidate.event.user - base) as usize];
            }
        };
        for (key, mut candidate) in bucket.packed {
            // Bucket assignment consumes the low hash bits. Use the high bits for slots.
            let mut slot = (mix(key).rotate_right(16)) & (capacity - 1);
            loop {
                let position = index[slot];
                if position == usize::MAX {
                    canonical(&mut candidate);
                    index[slot] = winners.len();
                    winners.push((key, candidate));
                    break;
                }
                if winners[position].0 == key {
                    if candidate.ingest < winners[position].1.ingest {
                        canonical(&mut candidate);
                        winners[position].1 = candidate;
                    }
                    break;
                }
                slot = (slot + 1) & (capacity - 1);
            }
        }
        for (key, mut candidate) in bucket.text {
            match others.entry(key) {
                Entry::Vacant(entry) => {
                    canonical(&mut candidate);
                    entry.insert(candidate);
                }
                Entry::Occupied(mut entry) if candidate.ingest < entry.get().ingest => {
                    canonical(&mut candidate);
                    entry.insert(candidate);
                }
                _ => {}
            }
        }
    }
    winners
        .into_iter()
        .map(|(_, v)| v)
        .chain(others.into_values())
        .filter(|v| v.event.ts != INVALID)
        .map(|v| v.event)
        .collect()
}

pub(super) fn read_events(
    dir: &Path,
    start: i64,
    end: i64,
    users: &mut Names,
    products: &HashMap<&str, u32>,
) -> io::Result<Vec<Event>> {
    let mut maps = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_none_or(|ext| ext != "csv") {
            continue;
        }
        let file = File::open(path)?;
        if file.metadata()?.len() > 0 {
            // The pipeline's input is mounted read-only and remains immutable for the run.
            maps.push(unsafe { Mmap::map(&file)? });
        }
    }
    let mut chunks = Vec::new();
    for map in &maps {
        let Some(header) = memchr::memchr(b'\n', map) else {
            continue;
        };
        let mut body = &map[header + 1..];
        while !body.is_empty() {
            let end = if body.len() <= CHUNK {
                body.len()
            } else {
                memchr::memchr(b'\n', &body[CHUNK..]).map_or(body.len(), |nl| CHUNK + nl + 1)
            };
            chunks.push(&body[..end]);
            body = &body[end..];
        }
    }
    let partitions = chunks.len().clamp(1, 256).next_power_of_two();
    let base = u32::try_from(users.names.len()).expect("too many distinct names");
    let catalog = Catalog {
        start,
        end,
        users,
        products,
    };
    let batches: Vec<_> = chunks
        .par_iter()
        .map(|chunk| {
            let text = std::str::from_utf8(chunk)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(parse_chunk(text, partitions, &catalog))
        })
        .collect::<io::Result<_>>()?;
    let mut translations = Vec::with_capacity(batches.len());
    let mut partitioned: Vec<Vec<_>> = (0..partitions).map(|_| Vec::new()).collect();
    for (chunk, batch) in batches.into_iter().enumerate() {
        translations.push(batch.unresolved.into_iter().map(|s| users.id(s)).collect());
        for (parts, bucket) in partitioned.iter_mut().zip(batch.buckets) {
            if !bucket.packed.is_empty() || !bucket.text.is_empty() {
                parts.push((chunk, bucket));
            }
        }
    }
    let groups: Vec<Vec<Event>> = partitioned
        .into_par_iter()
        .map(|b| dedup(b, base, &translations))
        .collect();
    let mut events = Vec::with_capacity(groups.iter().map(Vec::len).sum());
    for mut group in groups {
        events.append(&mut group);
    }
    Ok(events)
}
