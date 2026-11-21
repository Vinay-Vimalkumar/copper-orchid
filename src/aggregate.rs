use crate::model::{CampaignDay, Input, UserDay};
use ahash::AHashMap as HashMap;
use rayon::prelude::*;
use std::ops::Range;

const DAY: i64 = 86_400_000;
const SESSION_GAP: i64 = 1_800_000;
const LOOKBACK: i64 = 7 * DAY;

#[derive(Clone, Copy, Default)]
struct Counts {
    sessions: i64,
    purchasing_sessions: i64,
    revenue_cents: i64,
    assisted_sessions: i64,
}

impl Counts {
    fn add(&mut self, other: Self) {
        self.sessions += other.sessions;
        self.purchasing_sessions += other.purchasing_sessions;
        self.revenue_cents += other.revenue_cents;
        self.assisted_sessions += other.assisted_sessions;
    }
}

#[derive(Default)]
struct DayTotals {
    dense: Vec<Counts>,
    sparse: HashMap<u32, Counts>,
}

impl DayTotals {
    fn campaign(&mut self, id: u32) -> &mut Counts {
        if self.dense.is_empty() {
            self.sparse.entry(id).or_default()
        } else {
            &mut self.dense[id as usize]
        }
    }

    fn entries(self) -> impl Iterator<Item = (u32, Counts)> {
        self.dense
            .into_iter()
            .enumerate()
            .filter_map(|(id, counts)| {
                (counts.sessions > 0 || counts.assisted_sessions > 0).then_some((id as u32, counts))
            })
            .chain(self.sparse)
    }

    fn merge(&mut self, other: Self) {
        if !self.dense.is_empty() && !other.dense.is_empty() {
            for (counts, partial) in self.dense.iter_mut().zip(other.dense) {
                counts.add(partial);
            }
        } else {
            for (id, partial) in other.entries() {
                self.campaign(id).add(partial);
            }
        }
    }
}

type CampaignTotals = HashMap<i32, DayTotals>;

struct Attribution {
    consumed: usize,
    active: Vec<u32>,
    latest_by_campaign: Vec<Option<i64>>,
}

impl Attribution {
    fn new(campaigns: usize) -> Self {
        Self {
            consumed: 0,
            active: Vec::new(),
            latest_by_campaign: vec![None; campaigns],
        }
    }

    fn start_person(&mut self) {
        self.consumed = 0;
        for campaign in self.active.drain(..) {
            self.latest_by_campaign[campaign as usize] = None;
        }
    }

    fn record_session(
        &mut self,
        history: &[Touch],
        start: i64,
        revenue: i64,
        totals: &mut DayTotals,
    ) {
        let cutoff = start - LOOKBACK;
        while let Some(click) = history.get(self.consumed) {
            if click.ts > start {
                break;
            }
            let previous = history[..self.consumed].last();
            self.consumed += 1;
            if click.ts >= cutoff
                && !previous.is_some_and(|prev| prev.ts == click.ts && prev.id == click.id)
            {
                let latest = &mut self.latest_by_campaign[click.campaign as usize];
                if latest.is_none() {
                    self.active.push(click.campaign);
                }
                *latest = Some(click.ts);
            }
        }

        // One entry per campaign, including when many of its clicks share a timestamp.
        self.active.retain(|&campaign| {
            let latest = &mut self.latest_by_campaign[campaign as usize];
            if latest.is_some_and(|ts| ts >= cutoff) {
                totals.campaign(campaign).assisted_sessions += 1;
                true
            } else {
                *latest = None;
                false
            }
        });

        let credit = history[..self.consumed]
            .last()
            .filter(|click| click.ts >= cutoff)
            .map_or(0, |click| click.campaign);
        let row = totals.campaign(credit);
        row.sessions += 1;
        row.purchasing_sessions += i64::from(revenue > 0);
        row.revenue_cents += revenue;
    }
}

#[derive(Clone, Copy, Default)]
struct Action {
    ts: i64,
    category: u32,
    price: u32,
}

#[derive(Clone, Copy, Default)]
struct Touch {
    ts: i64,
    id: crate::model::ClickId,
    campaign: u32,
}

// One independently written counting-sort block per source stripe. After scatter,
// ends[user] is the exclusive end of that person's contiguous range within rows.
struct PersonGroups<T> {
    rows: Vec<T>,
    ends: Vec<usize>,
}

impl<T> PersonGroups<T> {
    fn person(&self, user: usize) -> &[T] {
        let start = if user == 0 { 0 } else { self.ends[user - 1] };
        &self.rows[start..self.ends[user]]
    }
}

fn group_people<'a, T: Sync, R: Copy + Default + Send>(
    rows: &'a [T],
    users: usize,
    user_of: impl Fn(&T) -> u32 + Sync,
    compact: impl Fn(&'a T) -> R + Sync,
) -> Vec<PersonGroups<R>> {
    let stripe = rows
        .len()
        .div_ceil(rayon::current_num_threads())
        .max(32_768);
    rows.par_chunks(stripe)
        .map(|source| {
            let mut ends = vec![0; users];
            for row in source {
                ends[user_of(row) as usize] += 1;
            }
            let mut offset = 0;
            for count in &mut ends {
                let length = *count;
                *count = offset;
                offset += length;
            }
            let mut rows = vec![R::default(); source.len()];
            for row in source {
                let cursor = &mut ends[user_of(row) as usize];
                rows[*cursor] = compact(row);
                *cursor += 1;
            }
            PersonGroups { rows, ends }
        })
        .collect()
}

fn aggregate_people(
    people: Range<usize>,
    event_groups: &[PersonGroups<Action>],
    click_groups: &[PersonGroups<Touch>],
    other_ids: &[Box<str>],
    categories: &[String],
    campaign_count: usize,
) -> (Vec<UserDay>, CampaignTotals) {
    let mut user_rows = Vec::new();
    let mut campaign_totals = HashMap::new();
    let mut attribution = Attribution::new(campaign_count);
    let mut category_revenue: HashMap<u32, i64> = HashMap::new();
    let mut events = Vec::new();
    let mut history = Vec::new();
    // Bound dense counters to 2 MiB per partition on wider calendars and catalogs.
    let dense_days = 65_536 / campaign_count;
    for user in people {
        events.clear();
        for group in event_groups {
            events.extend_from_slice(group.person(user));
        }
        if events.is_empty() {
            continue;
        }
        events.sort_unstable_by_key(|e| e.ts);
        history.clear();
        for group in click_groups {
            history.extend_from_slice(group.person(user));
        }
        // Reverse ID order on equal timestamps places the winning click last.
        history.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| b.id.cmp(a.id, other_ids)));
        attribution.start_person();
        let mut sessions = events
            .chunk_by(|a, b| b.ts - a.ts <= SESSION_GAP)
            .map(|session| (session[0].ts.div_euclid(DAY) as i32, session))
            .peekable();

        while let Some(&(day, _)) = sessions.peek() {
            let dense_len = if campaign_totals.len() < dense_days {
                campaign_count
            } else {
                0
            };
            let daily = campaign_totals.entry(day).or_insert_with(|| DayTotals {
                dense: vec![Counts::default(); dense_len],
                ..DayTotals::default()
            });
            let mut row = UserDay {
                user: user as u32,
                day,
                ..UserDay::default()
            };
            while let Some((_, session)) = sessions.next_if(|&(session_day, _)| session_day == day)
            {
                let mut revenue = 0;
                for event in session.iter().filter(|event| event.price > 0) {
                    let amount = i64::from(event.price);
                    revenue += amount;
                    *category_revenue.entry(event.category).or_default() += amount;
                }
                row.sessions += 1;
                row.events += session.len() as i64;
                row.revenue_cents += revenue;
                attribution.record_session(&history, session[0].ts, revenue, daily);
            }
            row.top_category = category_revenue
                .drain()
                .max_by(|(a, left), (b, right)| {
                    left.cmp(right)
                        .then_with(|| categories[*b as usize].cmp(&categories[*a as usize]))
                })
                .map(|(category, _)| category);
            user_rows.push(row);
        }
    }
    (user_rows, campaign_totals)
}

pub fn aggregate(input: &mut Input) -> (Vec<UserDay>, Vec<CampaignDay>) {
    if input.events.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let people = input.users.len();
    let (events, clicks) = rayon::join(
        || {
            group_people(
                &input.events,
                people,
                |e| e.user,
                |e| Action {
                    ts: e.ts,
                    category: e.category,
                    price: e.price,
                },
            )
        },
        || {
            group_people(
                &input.clicks,
                people,
                |c| c.user,
                |c| Touch {
                    ts: c.ts,
                    id: c.id,
                    campaign: c.campaign,
                },
            )
        },
    );
    let workers = rayon::current_num_threads();
    let batch_size = people.div_ceil(workers * 2).max(256);
    let partials: Vec<_> = (0..people.div_ceil(batch_size))
        .into_par_iter()
        .map(|batch| {
            aggregate_people(
                batch * batch_size..((batch + 1) * batch_size).min(people),
                &events,
                &clicks,
                &input.other_click_ids,
                &input.categories,
                input.campaigns.len(),
            )
        })
        .collect();
    let user_count: usize = partials.iter().map(|(rows, _)| rows.len()).sum();
    let mut partials = partials.into_iter();
    let (mut users, mut campaigns) = partials.next().unwrap();
    users.reserve(user_count - users.len());
    for (rows, totals) in partials {
        users.extend(rows);
        for (day, partial) in totals {
            use std::collections::hash_map::Entry;
            match campaigns.entry(day) {
                Entry::Vacant(entry) => {
                    entry.insert(partial);
                }
                Entry::Occupied(mut entry) => entry.get_mut().merge(partial),
            }
        }
    }
    let rows = campaigns
        .into_iter()
        .flat_map(|(day, totals)| {
            totals.entries().map(move |(campaign, counts)| CampaignDay {
                campaign,
                day,
                sessions: counts.sessions,
                purchasing_sessions: counts.purchasing_sessions,
                revenue_cents: counts.revenue_cents,
                assisted_sessions: counts.assisted_sessions,
            })
        })
        .collect();
    (users, rows)
}
