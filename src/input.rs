use crate::model::Input;
use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::io;
use std::path::Path;

const DAY: i64 = 86_400_000;

fn read_shards(dir: &Path) -> io::Result<Vec<String>> {
    let mut shards = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "csv") {
            shards.push(fs::read_to_string(path)?);
        }
    }
    Ok(shards)
}

fn lines(text: &str) -> impl Iterator<Item = &str> {
    // str::lines would discard a CR before LF; CR is field data here.
    text.split('\n').skip(1)
}

fn fields<const N: usize>(line: &str) -> Option<[&str; N]> {
    let mut fields = [""; N];
    let mut raw = line.split(',');
    for slot in &mut fields {
        let value = raw.next()?;
        let value = match value.strip_prefix('"') {
            Some(quoted) => quoted.strip_suffix('"')?,
            None => value,
        };
        if value.as_bytes().contains(&b'"') {
            return None;
        }
        *slot = value;
    }
    raw.next().is_none().then_some(fields)
}

fn decimal(digits: &[u8]) -> Option<i64> {
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0i64, |n, &digit| {
        let d = digit.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        n.checked_mul(10)?.checked_add(i64::from(d))
    })
}

fn days_before_year(year: i64) -> i64 {
    let y = year - 1;
    365 * y + y / 4 - y / 100 + y / 400
}

#[path = "identity.rs"]
mod identity;
use identity::Names;

struct Component {
    parent: u32,
    size: u32,
    smallest: u32,
}

fn root(sets: &mut [Component], mut node: u32) -> u32 {
    while sets[node as usize].parent != node {
        let parent = sets[node as usize].parent as usize;
        sets[node as usize].parent = sets[parent].parent;
        node = sets[parent].parent;
    }
    node
}

fn linked_users(dir: &Path) -> io::Result<Names> {
    let mut users = Names::default();
    let mut sets: Vec<Component> = Vec::new();
    for shard in read_shards(dir)? {
        for [a, b] in lines(&shard).filter_map(fields::<2>) {
            if a.is_empty() || b.is_empty() {
                continue;
            }
            let a = users.id(a);
            let b = users.id(b);
            while sets.len() < users.names.len() {
                let id = sets.len() as u32;
                sets.push(Component {
                    parent: id,
                    size: 1,
                    smallest: id,
                });
            }
            let mut a = root(&mut sets, a) as usize;
            let mut b = root(&mut sets, b) as usize;
            if a == b {
                continue;
            }
            if sets[a].size < sets[b].size {
                std::mem::swap(&mut a, &mut b);
            }
            sets[b].parent = a as u32;
            sets[a].size += sets[b].size;
            if users.names[sets[b].smallest as usize] < users.names[sets[a].smallest as usize] {
                sets[a].smallest = sets[b].smallest;
            }
        }
    }
    for id in users.index.values_mut() {
        let component = root(&mut sets, *id) as usize;
        *id = sets[component].smallest;
    }
    Ok(users)
}

fn window(data: &Path) -> Result<(i64, i64), Box<dyn Error>> {
    let json: serde_json::Value = serde_json::from_slice(&fs::read(data.join("manifest.json"))?)?;
    let get = |key: &str| {
        json[key]
            .as_str()
            .and_then(|text| events::Calendar::default().parse(text))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("invalid {key}")))
    };
    Ok((get("window_start")?, get("window_end")?))
}

#[path = "events.rs"]
mod events;
#[path = "clicks.rs"]
mod fast_clicks;
#[path = "shards.rs"]
mod shards;

pub fn read(data: &Path) -> Result<Input, Box<dyn Error>> {
    let began = std::time::Instant::now();
    let (start, end) = window(data)?;
    let mut users = linked_users(&data.join("links"))?;
    let mut categories = Names::default();
    categories.id("unknown");
    let product_text = fs::read_to_string(data.join("products.csv"))?;
    let products = lines(&product_text)
        .filter_map(fields::<2>)
        .map(|[product, category]| (product, categories.id(category)))
        .collect();

    let setup = std::time::Instant::now();
    let events = events::read_events(&data.join("events"), start, end, &mut users, &products)?;

    let events_done = std::time::Instant::now();
    let mut campaigns = vec![("organic".to_owned(), "organic".to_owned())];
    let campaign_text = fs::read_to_string(data.join("campaigns.csv"))?;
    let mut campaign_index = HashMap::new();
    for [id, channel] in lines(&campaign_text).filter_map(fields::<2>) {
        campaign_index.insert(id, u32::try_from(campaigns.len())?);
        campaigns.push((id.to_owned(), channel.to_owned()));
    }
    let clicks = fast_clicks::read_clicks(
        &data.join("clicks"),
        start,
        end,
        &mut users,
        &campaign_index,
    )?;

    if std::env::var_os("SHOPPING_TIMINGS").is_some() {
        eprintln!(
            "dimensions_links={:?} events={:?} clicks={:?}",
            setup - began,
            events_done - setup,
            events_done.elapsed()
        );
    }
    Ok(Input {
        events,
        clicks,
        users: users.names,
        categories: categories
            .names
            .into_iter()
            .map(|s| s.to_string())
            .collect(),
        campaigns,
    })
}
