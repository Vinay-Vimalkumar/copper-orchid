use super::events::Calendar;
use super::shards::Shards;
use super::{DAY, Names};
use crate::model::{Click, ClickId};
use rayon::prelude::*;
use std::collections::HashMap;
use std::{io, path::Path};

pub(super) fn read_clicks(
    dir: &Path,
    start: i64,
    end: i64,
    users: &mut Names,
    campaigns: &HashMap<&str, u32>,
) -> io::Result<(Vec<Click>, Vec<Box<str>>)> {
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
            let mut other_ids = Vec::new();
            let mut rows = Vec::with_capacity(chunk.len() / 52);
            crate::csv::rows::<4>(chunk, |[id, user, campaign, raw_ts]| {
                if user.is_empty() || id.is_empty() {
                    return;
                }
                let Some(&campaign) = campaign_lookup.get(campaign) else {
                    return;
                };
                let Some(ts) = calendar
                    .parse(raw_ts)
                    .filter(|&ts| min_ts <= ts && ts < end)
                else {
                    return;
                };
                let user = known.get(user).copied().unwrap_or_else(|| {
                    pending.push((rows.len(), user));
                    0
                });
                // Identical copies become adjacent in the per-person attribution sort.
                let id = ClickId::pack(id).unwrap_or_else(|| {
                    other_ids.push((rows.len(), id));
                    ClickId::default()
                });
                rows.push(Click {
                    user,
                    ts,
                    campaign,
                    id,
                });
            });
            Ok((rows, pending, other_ids))
        })
        .collect::<io::Result<_>>()?;
    let mut clicks = Vec::with_capacity(parsed.iter().map(|(rows, _, _)| rows.len()).sum());
    let mut other_click_ids = Vec::new();
    let mut index = ahash::AHashMap::new();
    for (mut rows, pending, other_ids) in parsed {
        for (row, id) in other_ids {
            rows[row].id = *index.entry(id).or_insert_with(|| {
                let key = ClickId::other(other_click_ids.len());
                other_click_ids.push(id.into());
                key
            });
        }
        for (index, name) in pending {
            rows[index].user = users.id(name);
        }
        clicks.append(&mut rows);
    }
    Ok((clicks, other_click_ids))
}
