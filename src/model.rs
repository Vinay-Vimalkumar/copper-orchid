pub struct Input {
    pub events: Vec<Event>,
    pub clicks: Vec<Click>,
    pub other_click_ids: Vec<Box<str>>,
    pub users: Vec<compact_str::CompactString>,
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

// Short k-prefixed IDs compare in byte order as a big-endian integer. Other
// spellings keep their complete UTF-8 value in the click dictionary.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct ClickId(u64);

impl ClickId {
    const OTHER: u64 = 1 << 63;

    pub fn pack(text: &str) -> Option<Self> {
        let [b'k', tail @ ..] = text.as_bytes() else {
            return None;
        };
        if tail.last() == Some(&0) || tail.first().is_some_and(|v| !v.is_ascii()) {
            return None;
        }
        let word = match tail.len() {
            8 => tail.try_into().unwrap(),
            0..=7 => {
                let mut bytes = [0; 8];
                bytes[..tail.len()].copy_from_slice(tail);
                bytes
            }
            _ => return None,
        };
        Some(Self(u64::from_be_bytes(word)))
    }

    pub fn other(index: usize) -> Self {
        Self(Self::OTHER | index as u64)
    }

    fn bytes<'a>(self, dict: &'a [Box<str>], buf: &'a mut [u8; 9]) -> &'a [u8] {
        if self.0 & Self::OTHER != 0 {
            return dict[(self.0 ^ Self::OTHER) as usize].as_bytes();
        }
        buf[0] = b'k';
        buf[1..].copy_from_slice(&self.0.to_be_bytes());
        &buf[..9 - (self.0.trailing_zeros() / 8) as usize]
    }

    pub fn cmp(self, other: Self, dict: &[Box<str>]) -> std::cmp::Ordering {
        if (self.0 | other.0) & Self::OTHER == 0 {
            self.0.cmp(&other.0)
        } else {
            self.bytes(dict, &mut [0; 9])
                .cmp(other.bytes(dict, &mut [0; 9]))
        }
    }
}

pub struct Click {
    pub user: u32,
    pub ts: i64,
    pub campaign: u32,
    pub id: ClickId,
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
