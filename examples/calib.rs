// Measures this machine's unit costs for back-of-envelope estimates (docs/profiling.md, "Back-of-envelope").
// Run: cargo run --release --example calib   (pin it like the benchmarks: taskset -c 2 target/release/examples/calib)
use std::hint::black_box;
use std::time::Instant;

// Best of 7: the least disturbed run is closest to the hardware's cost.
fn best<F: FnMut()>(mut work: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..7 {
        let start = Instant::now();
        work();
        best = best.min(start.elapsed().as_secs_f64());
    }
    best
}

fn main() {
    // Copy bandwidth: 256 KB fits L2, 4 MB fits L3, 64 MB is RAM.
    for size in [256 << 10, 4 << 20, 64 << 20] {
        let src = vec![1u8; size];
        let mut dst = vec![0u8; size];
        let reps = (1usize << 30) / size; // copy 1 GB in total
        let secs = best(|| {
            for _ in 0..reps {
                dst.copy_from_slice(black_box(&src));
                black_box(&dst);
            }
        });
        println!("memcpy {:>6} KB:               {:5.1} GB/s", size >> 10, (size * reps) as f64 / secs / 1e9);
    }

    // decode's fixed-width loop: bytes -> Vec<i64>.
    let n = 1 << 20;
    let bytes = vec![7u8; 8 * n];
    let secs = best(|| {
        let values: Vec<i64> = bytes
            .chunks_exact(8)
            .map(|piece| i64::from_le_bytes(piece.try_into().unwrap()))
            .collect();
        black_box(values);
    });
    println!("decode i64 (8 MB):               {:5.1} GB/s of input", (8 * n) as f64 / secs / 1e9);

    // One row group of Strings created then freed, repeated so the heap is reused (like a scan).
    let group = 65_536;
    let reps = 30;
    let secs = best(|| {
        for _ in 0..reps {
            let values: Vec<String> = (0..group).map(|_| black_box("MAIL").to_string()).collect();
            black_box(&values);
        }
    });
    println!("String create + free:            {:5.1} ns each", secs / (group * reps) as f64 * 1e9);

    // The same plus one clone of each, both freed (the old Vector::slice).
    let secs = best(|| {
        for _ in 0..reps {
            let values: Vec<String> = (0..group).map(|_| black_box("MAIL").to_string()).collect();
            let clones: Vec<String> = values.iter().cloned().collect();
            black_box((&values, &clones));
        }
    });
    println!("String create + clone + 2 frees: {:5.1} ns per value", secs / (group * reps) as f64 * 1e9);

    // UTF-8 check: per small value (as decode does) vs once over a whole buffer.
    let n = 1_000_000;
    let text = vec![b'a'; 12 * n];
    let secs = best(|| {
        for i in 0..n {
            black_box(std::str::from_utf8(black_box(&text[i * 12..i * 12 + 12])).unwrap());
        }
    });
    println!("from_utf8 per 12 B value:        {:5.1} ns each", secs / n as f64 * 1e9);
    let secs = best(|| {
        black_box(std::str::from_utf8(black_box(&text)).unwrap());
    });
    println!("from_utf8 over 12 MB at once:    {:5.1} GB/s", text.len() as f64 / secs / 1e9);

    // Page faults: a fresh large zeroed allocation is mapped lazily; the first write to each
    // 4 KB page faults (the kernel zeroes the page).
    let size = 64 << 20;
    let secs = best(|| {
        let mut buffer = vec![0u8; size];
        for i in (0..size).step_by(4096) {
            buffer[i] = 1;
        }
        black_box(&buffer);
    });
    println!("page fault (first touch, 4 KB):  {:5.0} ns each", secs / (size / 4096) as f64 * 1e9);
}
