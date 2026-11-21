use crate::model::{CampaignDay, Click, Input, UserDay};
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
            self.consumed += 1;
            if click.ts >= cutoff {
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

pub fn aggregate(input: &mut Input) -> (Vec<UserDay>, Vec<CampaignDay>) {
    input.events.sort_unstable_by_key(|e| (e.user, e.ts));
    // With reverse byte order for equal timestamps, the last eligible click always wins.
    input.clicks.sort_unstable_by(|a, b| {
        (a.user, a.ts)
            .cmp(&(b.user, b.ts))
            .then_with(|| b.id.cmp(&a.id))
    });

    let mut user_rows = Vec::new();
    let mut campaign_totals = HashMap::new();
    let mut attribution = Attribution::new(input.campaigns.len());
    let mut category_revenue: HashMap<u32, i64> = HashMap::new();
    let mut click_groups = input.clicks.chunk_by(|a, b| a.user == b.user).peekable();

    for events in input.events.chunk_by(|a, b| a.user == b.user) {
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
                    left.cmp(right).then_with(|| {
                        input.categories[*b as usize].cmp(&input.categories[*a as usize])
                    })
                })
                .map(|(category, _)| category);
            user_rows.push(row);
        }
    }
    (user_rows, campaign_totals.into_values().collect())
}
