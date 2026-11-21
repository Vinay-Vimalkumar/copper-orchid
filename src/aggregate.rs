use crate::model::{CampaignDay, Click, Event, Input, UserDay};
use std::collections::HashMap;

const DAY: i64 = 86_400_000;
const SESSION_GAP: i64 = 1_800_000;
const LOOKBACK: i64 = 7 * DAY;

type CampaignTotals = HashMap<(u32, i32), CampaignDay>;

fn campaign_row(totals: &mut CampaignTotals, campaign: u32, day: i32) -> &mut CampaignDay {
    totals
        .entry((campaign, day))
        .or_insert_with(|| CampaignDay {
            campaign,
            day,
            ..CampaignDay::default()
        })
}

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
        history: &[Click],
        start: i64,
        day: i32,
        revenue: i64,
        totals: &mut CampaignTotals,
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
                campaign_row(totals, campaign, day).assisted_sessions += 1;
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
        let row = campaign_row(totals, credit, day);
        row.sessions += 1;
        row.purchasing_sessions += i64::from(revenue > 0);
        row.revenue_cents += revenue;
    }
}

fn aggregate_people(
    events: &mut [Event],
    clicks: &mut [Click],
    categories: &[String],
    campaign_count: usize,
) -> (Vec<UserDay>, CampaignTotals) {
    events.sort_unstable_by_key(|e| (e.user, e.ts));
    // With reverse byte order for equal timestamps, the last eligible click always wins.
    clicks.sort_unstable_by(|a, b| {
        (a.user, a.ts)
            .cmp(&(b.user, b.ts))
            .then_with(|| b.id.cmp(&a.id))
    });

    let mut user_rows = Vec::new();
    let mut campaign_totals = HashMap::new();
    let mut attribution = Attribution::new(campaign_count);
    let mut category_revenue: HashMap<u32, i64> = HashMap::new();
    let mut click_groups = clicks.chunk_by(|a, b| a.user == b.user).peekable();

    for events in events.chunk_by(|a, b| a.user == b.user) {
        let user = events[0].user;
        while click_groups
            .peek()
            .is_some_and(|group| group[0].user < user)
        {
            click_groups.next();
        }
        let history = click_groups
            .next_if(|group| group[0].user == user)
            .unwrap_or(&[]);
        attribution.start_person();
        let mut sessions = events
            .chunk_by(|a, b| b.ts - a.ts <= SESSION_GAP)
            .map(|session| (session[0].ts.div_euclid(DAY) as i32, session))
            .peekable();

        while let Some(&(day, _)) = sessions.peek() {
            let mut row = UserDay {
                user,
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
                attribution.record_session(
                    history,
                    session[0].ts,
                    day,
                    revenue,
                    &mut campaign_totals,
                );
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

// Divide the occupied storage in place. Each canonical ID goes to exactly one side.
fn partition_by<T>(items: &mut [T], below_cut: impl Fn(&T) -> bool) -> (&mut [T], &mut [T]) {
    let mut left = 0;
    let mut right = items.len();
    while left < right {
        if below_cut(&items[left]) {
            left += 1;
        } else {
            right -= 1;
            if below_cut(&items[right]) {
                items.swap(left, right);
                left += 1;
            }
        }
    }
    items.split_at_mut(left)
}

type PartialResult = (Vec<UserDay>, CampaignTotals);

fn reduce_people(
    events: &mut [Event],
    clicks: &mut [Click],
    categories: &[String],
    campaign_count: usize,
    results: &mut [Option<PartialResult>],
) {
    if results.len() > 1 && events.len() >= 16_384 {
        // Sample by number of events so a popular identity is represented in the split.
        let mut sample: [u32; 127] = std::array::from_fn(|i| events[(events.len() / 127) * i].user);
        let middle = sample.len() / 2;
        let cut = *sample.select_nth_unstable(middle).1;
        let (low_events, high_events) = partition_by(events, |event| event.user < cut);
        if !low_events.is_empty() && !high_events.is_empty() {
            let (low_clicks, high_clicks) = partition_by(clicks, |click| click.user < cut);
            let (low_results, high_results) = results.split_at_mut(results.len() / 2);
            rayon::join(
                || {
                    reduce_people(
                        low_events,
                        low_clicks,
                        categories,
                        campaign_count,
                        low_results,
                    )
                },
                || {
                    reduce_people(
                        high_events,
                        high_clicks,
                        categories,
                        campaign_count,
                        high_results,
                    )
                },
            );
            return;
        }
    }
    results[0] = Some(aggregate_people(events, clicks, categories, campaign_count));
}

pub fn aggregate(input: &mut Input) -> (Vec<UserDay>, Vec<CampaignDay>) {
    let workers = rayon::current_num_threads();
    let mut partials: Vec<_> = (0..workers).map(|_| None).collect();
    reduce_people(
        &mut input.events,
        &mut input.clicks,
        &input.categories,
        input.campaigns.len(),
        &mut partials,
    );
    let user_count: usize = partials.iter().flatten().map(|(rows, _)| rows.len()).sum();
    let mut partials = partials.into_iter().flatten();
    let (mut users, mut campaigns) = partials.next().unwrap();
    users.reserve(user_count - users.len());
    for (rows, totals) in partials {
        users.extend(rows);
        for partial in totals.into_values() {
            let row = campaign_row(&mut campaigns, partial.campaign, partial.day);
            row.sessions += partial.sessions;
            row.purchasing_sessions += partial.purchasing_sessions;
            row.revenue_cents += partial.revenue_cents;
            row.assisted_sessions += partial.assisted_sessions;
        }
    }
    (users, campaigns.into_values().collect())
}
