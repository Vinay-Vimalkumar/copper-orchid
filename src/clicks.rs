use super::events::Calendar;
use super::shards::Shards;
use super::{DAY, Names, fields};
use crate::model::Click;
use rayon::prelude::*;
use std::collections::HashMap;
use std::{io, path::Path};

pub(super) fn read_clicks(
    dir: &Path,
    start: i64,
    end: i64,
    users: &mut Names,
    campaigns: &HashMap<&str, u32>,
) -> io::Result<Vec<Click>> {
    let shards = Shards::open(dir)?;
    let chunks = shards.chunks(512 * 1024);
    let min_ts = start - 7 * DAY;
    let known = &users.index;
    // Campaign IDs come from the clean input dimension and may have any spelling.
    let campaign_lookup: ahash::AHashMap<_, _> = campaigns
        .iter()
        .map(|(&id, &campaign)| (id, campaign))
        .collect();
    let parsed: Vec<_> = chunks
        .par_iter()
        .map(|chunk| {
            let chunk = std::str::from_utf8(chunk)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let mut calendar = Calendar::default();
            let mut pending = Vec::new();
            let mut rows = Vec::with_capacity(chunk.len() / 52);
            for [id, user, campaign, raw_ts] in chunk.split('\n').filter_map(fields::<4>) {
                if user.is_empty() || id.is_empty() {
                    continue;
                }
                let Some(&campaign) = campaign_lookup.get(campaign) else {
                    continue;
                };
                let Some(ts) = calendar
                    .parse(raw_ts)
                    .filter(|&ts| min_ts <= ts && ts < end)
                else {
                    continue;
                };
                let user = known.get(user).copied().unwrap_or_else(|| {
                    pending.push((rows.len(), user));
                    0
                });
                // Identical copies become adjacent in the per-person attribution sort.
                rows.push(Click {
                    user,
                    ts,
                    campaign,
                    id: id.into(),
                });
            }
            Ok((rows, pending))
        })
        .collect::<io::Result<_>>()?;
    let mut clicks = Vec::with_capacity(parsed.iter().map(|(rows, _)| rows.len()).sum());
    for (mut rows, pending) in parsed {
        for (index, name) in pending {
            rows[index].user = users.id(name);
        }
        clicks.append(&mut rows);
    }
    Ok(clicks)
}
