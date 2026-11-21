mod aggregate;
mod input;
mod model;
mod output;

use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = env::args_os().skip(1);
    let data = PathBuf::from(
        args.next()
            .or_else(|| env::var_os("DATA_DIR"))
            .unwrap_or_else(|| "/data".into()),
    );
    let out = PathBuf::from(
        args.next()
            .or_else(|| env::var_os("OUTPUT_DIR"))
            .unwrap_or_else(|| "/output".into()),
    );
    if args.next().is_some() {
        return Err("usage: shopping-sessions [DATA_DIR [OUTPUT_DIR]]".into());
    }

    let start = Instant::now();
    let available = std::thread::available_parallelism().map_or(1, |n| n.get());
    let threads = env::var("NCPU")
        .ok()
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(available)
        .clamp(1, available.min(8));
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()?;
    let mut input = input::read(&data)?;
    let parsed = Instant::now();
    let (users, campaigns) = aggregate::aggregate(&mut input);
    let aggregated = Instant::now();
    output::write(&out, &input, &users, &campaigns)?;
    if env::var_os("SHOPPING_TIMINGS").is_some() {
        eprintln!(
            "read_dedup_identity={:?} aggregate={:?} parquet={:?} user_rows={} campaign_rows={}",
            parsed - start,
            aggregated - parsed,
            aggregated.elapsed(),
            users.len(),
            campaigns.len()
        );
    }
    Ok(())
}
