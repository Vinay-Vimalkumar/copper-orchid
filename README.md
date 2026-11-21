# Shopping Sessions

The Rust executable reads retail event, click and identity-link CSV shards. It resolves linked people, keeps the earliest ingested copy of each event, groups purchases and sessions by UTC start day, and writes the user_daily and campaign_daily Parquet tables.

Build and run with Cargo:

```sh
cargo build --locked --release
target/release/shopping-sessions /path/to/input /path/to/output
```

The Docker entrypoint accepts the same optional paths. It defaults to `DATA_DIR=/data` and `OUTPUT_DIR=/output`. Use a fresh output directory writable by UID 1000. Set `SHOPPING_TIMINGS=1` to report input, aggregation and Parquet write durations on stderr.

The CSV parsing, identity resolution, deduplication, session calculation and attribution code are original. Output uses the Apache Parquet Rust library. Manifest parsing uses serde_json. See Cargo.lock for dependency versions.
