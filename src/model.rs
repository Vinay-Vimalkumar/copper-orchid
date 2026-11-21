pub struct Input {
    pub events: Vec<Event>,
    pub clicks: Vec<Click>,
    pub users: Vec<String>,
    pub categories: Vec<String>,
    // Index zero represents an organic session.
    pub campaigns: Vec<(String, String)>,
}

pub struct Event {
    pub user: u32,
    pub ts: i64,
    pub category: u32,
    // A valid purchase always has a positive price.
    pub price: u32,
}

pub struct Click {
    pub user: u32,
    pub ts: i64,
    pub campaign: u32,
    pub id: compact_str::CompactString,
}

#[derive(Default)]
pub struct UserDay {
    pub user: u32,
    pub day: i32,
    pub sessions: i64,
    pub events: i64,
    pub revenue_cents: i64,
    pub top_category: Option<u32>,
}

#[derive(Default)]
pub struct CampaignDay {
    pub campaign: u32,
    pub day: i32,
    pub sessions: i64,
    pub purchasing_sessions: i64,
    pub revenue_cents: i64,
    pub assisted_sessions: i64,
}
