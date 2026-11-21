use super::shards::Shards;
use super::{DAY, Names, days_before_year, decimal};
use crate::model::Event;
use rayon::prelude::*;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

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
pub(super) struct Calendar {
    days: [Option<(u64, i64)>; 32],
}

impl Calendar {
    #[inline]
    fn midnight(&mut self, b: &[u8]) -> Option<i64> {
        let day = pair(b, 8)? as usize;
        if !(1..32).contains(&day) {
            return None;
        }
        let prefix = u64::from_ne_bytes(b[..8].try_into().unwrap());
        Some(match self.days[day] {
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
        })
    }

    #[inline]
    pub(super) fn parse(&mut self, text: &str) -> Option<i64> {
        let b = text.as_bytes();
        if !matches!(b.len(), 20 | 25) || b[4] != b'-' {
            if let [prefix @ .., a, b, c, d, e] = b
                && prefix.len() == 8
            {
                let word = u64::from_le_bytes(prefix.try_into().unwrap());
                let high = 0xf0f0_f0f0_f0f0_f0f0;
                let ascii_zero = 0x3030_3030_3030_3030;
                if word & high != ascii_zero
                    || word.wrapping_add(0x0606_0606_0606_0606) & high != ascii_zero
                {
                    return None;
                }
                let digits = word & !high;
                let pairs = (digits.wrapping_mul(10) + (digits >> 8)) & 0x00ff_00ff_00ff_00ff;
                let groups = (pairs.wrapping_mul(100) + (pairs >> 16)) & 0x0000_ffff_0000_ffff;
                let prefix = (groups as u32 as i64) * 10_000 + (groups >> 32) as i64;
                let suffix = [a, b, c, d, e].into_iter().try_fold(0, |acc, byte| {
                    let digit = byte.wrapping_sub(b'0');
                    (digit < 10).then(|| acc * 10 + i64::from(digit))
                })?;
                return Some(prefix * 100_000 + suffix);
            }
            return decimal(b);
        }
        if b[10] != b'T' || b[13] != b':' || b[16] != b':' {
            return None;
        }
        let midnight = self.midnight(b)?;
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
fn inline_id(id: &str, prefix: u8) -> Option<u64> {
    let b = id.as_bytes();
    if b.first() != Some(&prefix) || b.len() > 9 || b.last() == Some(&0) {
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
    ts: i64,
    user: u32,
    purchase: u32,
}

#[derive(Default)]
struct Bucket<'a> {
    packed: Vec<(u64, Candidate)>,
    text: Vec<(&'a str, Candidate)>,
}

struct Batch<'a> {
    buckets: Vec<Bucket<'a>>,
    unresolved: Vec<&'a str>,
    purchases: Vec<(u32, u32)>,
}

// Assign a shared provisional index to each common unlinked user. Occupied slots
// never move. Failed probes go to the per-chunk text dictionary, which is merged
// by full byte equality after the scan.
struct MissingUsers {
    slots: Vec<AtomicU64>,
}

impl MissingUsers {
    fn new(input_bytes: usize) -> Self {
        let capacity = (input_bytes / 1024).clamp(64, 1 << 23).next_power_of_two();
        Self {
            slots: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    #[inline]
    fn index(&self, user: &str) -> Option<u32> {
        let key = inline_id(user, b'u')?;
        if key == 0 {
            return None;
        }
        let mask = self.slots.len() - 1;
        let start = mix(key);
        for probe in 0..16 {
            let slot = start.wrapping_add(probe) & mask;
            let entry = &self.slots[slot];
            let mut existing = entry.load(Ordering::Relaxed);
            if existing == 0 {
                existing =
                    match entry.compare_exchange(0, key, Ordering::Relaxed, Ordering::Relaxed) {
                        Ok(_) => key,
                        Err(value) => value,
                    };
            }
            if existing == key {
                return Some(slot as u32);
            }
        }
        None
    }

    fn resolve(&self, users: &mut Names) -> Vec<u32> {
        let mut result = vec![0; self.slots.len()];
        for (slot, entry) in self.slots.iter().enumerate() {
            let key = entry.load(Ordering::Relaxed);
            if key == 0 {
                continue;
            }
            let suffix = key.to_le_bytes();
            let len = 8 - (key.leading_zeros() / 8) as usize;
            let mut name = [b'u'; 9];
            name[1..=len].copy_from_slice(&suffix[..len]);
            let text = std::str::from_utf8(&name[..=len]).unwrap();
            result[slot] = users.id(text);
        }
        result
    }
}

struct Catalog<'a> {
    start: i64,
    end: i64,
    users: &'a Names,
    shared: &'a MissingUsers,
    products: &'a HashMap<&'a str, u32>,
}

#[inline]
fn event_fields(line: &str) -> Option<[&str; 7]> {
    let bytes = line.as_bytes();
    let quote_count = memchr::memchr_iter(b'"', bytes).count();
    let mut quotes_used = 0;
    let mut start = 0;
    let mut values = [""; 7];
    let mut commas = memchr::memchr_iter(b',', bytes);
    for (index, value) in values.iter_mut().enumerate() {
        let end = if index < 6 {
            commas.next()?
        } else {
            if commas.next().is_some() {
                return None;
            }
            bytes.len()
        };
        let (lo, hi) = if end >= start + 2 && bytes[start] == b'"' && bytes[end - 1] == b'"' {
            quotes_used += 2;
            (start + 1, end - 1)
        } else {
            (start, end)
        };
        // Input is UTF-8. ASCII commas and quotes delimit character boundaries.
        *value = unsafe { line.get_unchecked(lo..hi) };
        start = end + 1;
    }
    (quote_count == quotes_used).then_some(values)
}

fn parse_chunk<'a>(chunk: &'a str, partitions: usize, catalog: &Catalog) -> Batch<'a> {
    let mut calendar = Calendar::default();
    let mut missing = HashMap::<&str, u32>::new();
    let mut unresolved = Vec::new();
    let mut purchases = Vec::new();
    let mut buckets: Vec<_> = (0..partitions)
        .map(|_| Bucket {
            packed: Vec::with_capacity(chunk.len() / 72 / partitions),
            text: Vec::new(),
        })
        .collect();
    let mask = partitions - 1;
    for line in chunk.split('\n') {
        let Some([id, user, raw_ts, ingest, kind, product, raw_price]) = event_fields(line) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        // Published ingests are F1. Also tolerate repeated headers in concatenated shards.
        if ingest.len() != 20 || !ingest.ends_with('Z') {
            continue;
        }
        let Some(ingest) = calendar.parse(ingest) else {
            continue;
        };
        let ts = calendar
            .parse(raw_ts)
            .filter(|&ts| catalog.start <= ts && ts < catalog.end && ingest - ts <= DAY);
        let price = match kind {
            "view" | "cart" => Some(0),
            "purchase" => decimal(raw_price.as_bytes()).filter(|v| (1..=10_000_000).contains(v)),
            _ => None,
        };
        let item = match (ts, price) {
            (Some(ts), Some(price)) if !user.is_empty() && !product.is_empty() => {
                let user = catalog.users.index.get(user).copied().unwrap_or_else(|| {
                    if let Some(slot) = catalog.shared.index(user) {
                        return u32::try_from(catalog.users.names.len() + slot as usize)
                            .expect("too many distinct names");
                    }
                    *missing.entry(user).or_insert_with(|| {
                        let id = u32::try_from(
                            catalog.users.names.len()
                                + catalog.shared.slots.len()
                                + unresolved.len(),
                        )
                        .expect("too many distinct names");
                        unresolved.push(user);
                        id
                    })
                });
                let purchase = if price == 0 {
                    0
                } else {
                    let category = catalog.products.get(product).copied().unwrap_or(0);
                    purchases.push((category, price as u32));
                    u32::try_from(purchases.len()).expect("too many purchases in one chunk")
                };
                Candidate {
                    ingest,
                    ts,
                    user,
                    purchase,
                }
            }
            // Invalid earliest copies must still suppress later valid copies.
            _ => Candidate {
                ingest,
                ts: INVALID,
                user: 0,
                purchase: 0,
            },
        };
        if let Some(key) = inline_id(id, b'e').filter(|&key| key != 0) {
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
        purchases,
    }
}

struct Translation {
    users: Vec<u32>,
    purchase_start: u32,
}

fn dedup(
    parts: Vec<(usize, Bucket<'_>)>,
    base: u32,
    shared: &[u32],
    translations: &[Translation],
    purchases: &[(u32, u32)],
) -> Vec<Event> {
    let count = parts.iter().map(|(_, b)| b.packed.len()).sum::<usize>();
    let capacity = (count + count / 2 + 1).next_power_of_two();
    let mut keys = vec![0u64; capacity];
    let mut index = vec![0u32; capacity];
    let mut winners: Vec<Candidate> = Vec::with_capacity(count);
    let mut others = HashMap::<&str, Candidate>::new();

    for (chunk, bucket) in parts {
        let canonical = |candidate: &mut Candidate| {
            if candidate.purchase != 0 {
                candidate.purchase += translations[chunk].purchase_start;
            }
            if candidate.ts != INVALID && candidate.user >= base {
                let delta = (candidate.user - base) as usize;
                candidate.user = if delta < shared.len() {
                    shared[delta]
                } else {
                    translations[chunk].users[delta - shared.len()]
                };
            }
        };
        for (key, mut candidate) in bucket.packed {
            // Bucket assignment consumes the low hash bits. Use the high bits for slots.
            let mut slot = (mix(key).rotate_right(16)) & (capacity - 1);
            loop {
                if keys[slot] == 0 {
                    canonical(&mut candidate);
                    keys[slot] = key;
                    index[slot] = u32::try_from(winners.len()).expect("too many partition winners");
                    winners.push(candidate);
                    break;
                }
                if keys[slot] == key {
                    let winner = &mut winners[index[slot] as usize];
                    if candidate.ingest < winner.ingest {
                        canonical(&mut candidate);
                        *winner = candidate;
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
    winners.extend(others.into_values());
    winners
        .into_iter()
        .filter(|v| v.ts != INVALID)
        .map(|v| {
            let (category, price) = purchases[v.purchase as usize];
            Event {
                ts: v.ts,
                user: v.user,
                category,
                price,
            }
        })
        .collect()
}

pub(super) fn read_events(
    dir: &Path,
    start: i64,
    end: i64,
    users: &mut Names,
    products: &HashMap<&str, u32>,
) -> io::Result<Vec<Event>> {
    let started = Instant::now();
    let maps = Shards::open(dir)?;
    let chunks = maps.chunks(CHUNK);
    let shared = MissingUsers::new(chunks.iter().map(|c| c.len()).sum());
    let partitions = chunks.len().clamp(1, 256).next_power_of_two();
    let base = u32::try_from(users.names.len()).expect("too many distinct names");
    let catalog = Catalog {
        start,
        end,
        users,
        shared: &shared,
        products,
    };
    let mapped = Instant::now();
    let batches: Vec<_> = chunks
        .par_iter()
        .map(|chunk| {
            let text = std::str::from_utf8(chunk)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(parse_chunk(text, partitions, &catalog))
        })
        .collect::<io::Result<_>>()?;
    let parsed = Instant::now();
    let known = shared.resolve(users);
    let mut translations = Vec::with_capacity(batches.len());
    let mut purchases = vec![(0, 0)];
    let mut partitioned: Vec<Vec<_>> = (0..partitions).map(|_| Vec::new()).collect();
    for (chunk, batch) in batches.into_iter().enumerate() {
        let purchase_start = u32::try_from(purchases.len() - 1).expect("too many purchases");
        purchases.extend(batch.purchases);
        assert!(
            purchases.len() - 1 <= u32::MAX as usize,
            "too many purchases"
        );
        translations.push(Translation {
            users: batch.unresolved.into_iter().map(|s| users.id(s)).collect(),
            purchase_start,
        });
        for (parts, bucket) in partitioned.iter_mut().zip(batch.buckets) {
            if !bucket.packed.is_empty() || !bucket.text.is_empty() {
                parts.push((chunk, bucket));
            }
        }
    }
    let translated = Instant::now();
    let groups: Vec<Vec<Event>> = partitioned
        .into_par_iter()
        .map(|b| dedup(b, base, &known, &translations, &purchases))
        .collect();
    let reduced = Instant::now();
    let total = groups.iter().map(Vec::len).sum();
    let mut events: Vec<Event> = Vec::with_capacity(total);
    let mut spare = &mut events.spare_capacity_mut()[..total];
    let mut copies = Vec::with_capacity(groups.len());
    for group in groups {
        let (dest, rest) = spare.split_at_mut(group.len());
        copies.push((group, dest));
        spare = rest;
    }
    copies.into_par_iter().for_each(|(group, dest)| {
        for (event, slot) in group.into_iter().zip(dest) {
            slot.write(event);
        }
    });
    // Every output slot belongs to exactly one completed copy.
    unsafe { events.set_len(total) }
    if std::env::var_os("SHOPPING_TIMINGS").is_some() {
        eprintln!(
            "event_mmap={:?} event_scan={:?} event_names={:?} event_dedup={:?} event_collect={:?}",
            mapped - started,
            parsed - mapped,
            translated - parsed,
            reduced - translated,
            reduced.elapsed()
        );
    }
    Ok(events)
}
