use std::hint::black_box;
use std::time::{Duration, Instant};

// Runs `work` once untimed (warm-up), then `runs` times timed, and prints min, median and
// throughput based on the median. `rows` and `bytes` are how much one run processes.
// `work` returns a value, e.g. a checksum: black_box stops the compiler from deleting work
// whose result is unused. The last run's value is returned so the caller can check it.
pub fn measure<T, F: FnMut() -> T>(
    name: &str,
    runs: usize,
    rows: usize,
    bytes: u64,
    mut work: F,
) -> T {
    assert!(runs > 0, "measure: runs must be at least 1");
    black_box(work()); // warm-up: fills the page cache, the allocator, the branch predictors

    let mut times: Vec<Duration> = Vec::with_capacity(runs);
    let mut result = None;
    for _ in 0..runs {
        let start = Instant::now();
        let value = black_box(work());
        times.push(start.elapsed());
        result = Some(value);
    }

    times.sort();
    let min = times[0];
    let median = times[runs / 2];
    let secs = median.as_secs_f64();
    println!(
        "{name}: min {min:.2?}, median {median:.2?}, {:.1} M rows/s, {:.0} MB/s",
        rows as f64 / secs / 1e6,
        bytes as f64 / secs / 1e6
    );
    result.unwrap() // runs > 0, so at least one value was stored
}
