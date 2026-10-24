# Profiling the engine with perf

A step-by-step routine for answering "why is this slow?" with evidence instead of guesses.
Each step says what to run, why, what you should see, and what it proves (and what it does not).

Every number below was measured on this machine (2026-10-24, SF0.1 lineitem, 600,572 rows,
warm page cache, pinned to P-core 2), on the code **after Fix 1** (`take_range` moves Strings
instead of cloning them). Where it helps, the number from before Fix 1 is shown too. Your runs
will differ by about ±10%; much bigger differences mean something changed and are worth a look.

The question we started with:

```
pax Q6 columns:  median   4.07 ms, 4133 MB/s
pax all columns: median 194.71 ms,  434 MB/s     ← why 10x worse per byte?   (before Fix 1)
```

## The method in one picture

```
1. measure          → a number                     (bench::measure)
2. estimate         → what the number should be    (back-of-envelope, see the BOE section)
3. gap?             → a question                   ("why 10x worse per byte?")
4. hypothesis       → what differs between fast and slow
5. experiment       → change ONE thing, measure    (proves or kills the hypothesis)
6. perf stat        → how much work, what kind     (instructions, IPC, misses, faults)
7. perf record      → where the work is            (functions, call stacks)
8. check            → profile numbers vs estimate  (do they agree?)
9. log              → write it down                (before/after comparison later)
```

Steps 1–5 need no profiler. perf comes after you know what question you are asking.

## The ways to run things

| command | what it does | use it for |
|---|---|---|
| `./target/release/olap` | CSV → PAX conversion, sizes, CSV vs PAX benchmarks for 4 column sets | Step 1, and to (re)create `lineitem.pax` |
| `./target/release/olap pax 8,9,13 [runs]` | only scans those PAX columns `runs` times (default 20) | Steps 4–13: one workload, nothing else |
| `cargo run --release --example calib` | measures this machine's unit costs | the BOE section |

`olap pax ...` reads the existing `data/sf0.1/lineitem.pax`, so run plain `olap` once first.

---

## Step 0: One-time checks

```sh
perf --version                                # this machine: 7.2.11
cat /proc/sys/kernel/perf_event_paranoid      # this machine: 2
cat /sys/devices/cpu_core/cpus                # P-cores: 0-15
cat /sys/devices/cpu_atom/cpus                # E-cores: 16-27
```

**Why.**
- `perf_event_paranoid = 2`: you can profile your own programs without sudo, but only user space.
  Kernel time (page faults, the page-cache copy inside `pread`) is not in `perf record` profiles.
  `perf stat` still shows the total kernel time as `seconds sys`.
- Since perf 7.2.11, `perf record --call-graph` asks for kernel call stacks too and fails at
  paranoid 2 with `Failure to open any events for recording`. Add **`--user-callchains`**.
- The CPU is hybrid: P-cores up to 5.5 GHz, E-cores up to 3.9 GHz. If the OS moves the program
  between them, times jump by ~30% for reasons unrelated to your code. Unpinned, perf also
  splits samples into two tables (`cpu_core` and `cpu_atom`).

**Fix: pin every benchmark and profile to one P-core** with `taskset -c 2`. Pinned, `perf stat`
still prints `<not counted> cpu_atom/...` lines: ignore them, nothing ran on an E-core.

**Proves:** nothing yet; it removes noise so later steps are trustworthy.

---

## Step 1: Measure

```sh
cargo build --release
taskset -c 2 ./target/release/olap
```

**Expected output (after Fix 1):**

```
csv Q6 columns:   median 313 ms,  241 MB/s     pax Q6 columns:   median   3.8 ms, 4386 MB/s
csv fixed only:   median 310 ms,  243 MB/s     pax fixed only:   median   2.7 ms, 4378 MB/s
csv varchar only: median 387 ms,  195 MB/s     pax varchar only: median  61.3 ms,  255 MB/s
csv all columns:  median 380 ms,  198 MB/s     pax all columns:  median 131.4 ms,  644 MB/s
  checksums ... match: true                    (each set)
```

Before Fix 1: pax varchar only 99.4 ms, pax all columns 194.7 ms.

**How `bench::measure` works.** One untimed warm-up run (page faults, malloc growing its heap,
cold instruction cache), then N timed runs; prints min and median.

- **min**: best case, least disturbed by the OS. **median**: typical; one unlucky run does not move it.
- **rows/s** = 600,572 / median. Same for every line, so formats compare fairly.
- **MB/s** = bytes / median. PAX: on-disk bytes of the requested columns (`PaxFile::columns_bytes`).
  CSV: the whole 75.4 MB file, because CSV must parse all of it.
- **checksum** = (rows, sum of l_quantity). Sets without column 4 only check rows: `(600572, 0.0)`.

**Inside the timed region** (one PAX run): `PaxFile::open`, 294 `Scan::next` calls, one
`read_chunk` per row group per column (`pread` + decode), the 2048-row `take_range`s, the `drain`
sum, and freeing everything. **Not inside:** disk I/O (the file was just written, so it is in the
page cache), the warm-up run, the file write.

**Proves:** how long the job takes, on this machine, warm. Nothing about *why*.

---

## Step 2: Estimate what it should be

Before looking at any profiler, predict the number from the work the code does and what each unit
of work costs on this machine. The full method, with this machine's measured costs and worked
examples, is in the **Back-of-envelope** section below. Short version for the original question:

- **Q6 (16.8 MB):** every byte is touched ~5 times (zeroing, kernel copy, decode, take, sum) at
  ~20–50 GB/s → ~2–4 ms. Measured ~4 ms: **bandwidth-bound, makes sense.**
- **All columns (84.6 MB):** 5× the bytes → ~20 ms. Measured 195 ms (before Fix 1).
  **~175 ms is not explained by bytes.**

**Proves:** there is a gap and how big it is. The question: *what do the extra columns cost
besides their bytes?*

---

## Step 3: Hypothesis — what differs between fast and slow?

lineitem has 11 fixed-width columns and 5 Varchar (`8, 9, 13, 14, 15`). Decoding a Varchar chunk
creates one `String` per row; before Fix 1, `slice` also cloned each one; every copy is freed later.

| | Q6 | all columns |
|---|---|---|
| Varchar columns | 0 | 5 |
| Strings created in `decode` | 0 | ≈ 3.0 M |
| Strings cloned in `slice` (before Fix 1) | 0 | ≈ 3.0 M |
| allocations per run | ~1.5 K | ~6 M before Fix 1, ~3 M after |

**Hypothesis:** the cost is per String (malloc + copy + free), not per byte.

**Proves:** nothing. It is a guess the next step can kill.

---

## Step 4: Experiment — change one thing

Scan sets with similar bytes but different String counts. Write predictions down *before* running.

```sh
taskset -c 2 ./target/release/olap pax 7,10,11,12
taskset -c 2 ./target/release/olap pax 8,9,13
taskset -c 2 ./target/release/olap pax 15
taskset -c 2 ./target/release/olap pax 8,9
taskset -c 2 ./target/release/olap pax 4,5,6,10
taskset -c 2 ./target/release/olap pax 0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15
```

| set | MB/scan | Strings/row | `olap pax`, before Fix 1 | `olap pax`, after Fix 1 | inside `olap`, after Fix 1 |
|---|---|---|---|---|---|
| fixed `7,10,11,12` | 12.0 | 0 | 3.8–4.5 ms | **3.6–3.9 ms** | 2.7 ms (see 4b) |
| varchar `8,9,13` | 15.6 | 3 | 101.6 ms | **66.5 ms** | 61.3 ms |
| comment `15` | 18.3 | 1 | 46.0 ms | **30.1 ms** | — |
| flags `8,9` | 6.0 | 2 | 60.6 ms | **39.2 ms** | — |
| Q6 `4,5,6,10` | 16.8 | 0 | 5.3 ms | **5.0–5.6 ms** | 3.8 ms (see 4b) |
| all `0..15` | 84.6 | 5 | 200.5 ms | **143.4 ms** | 131.4 ms |

**Result:**
- fixed vs varchar: 1.3× the bytes, ~18× the time (~25× before Fix 1).
- **flags** (two 1-character columns, 6 MB) take ~10× longer than four fixed columns with twice
  the bytes. Cost follows the String count, not the byte count.
- Fix 1 cut every Varchar set by about a third and left the fixed-width sets unchanged, which is
  what removing one malloc + free per Varchar value predicts.

**Proves:** *which input* causes the cost: Strings. **Does not prove** which code is responsible;
Steps 10–12 do that.

### 4b: The surprise — why are fixed-width sets slower when run alone?

Fixed-only takes 2.7 ms inside `olap` but ~3.6 ms with `olap pax`. Same code, so the difference is
the process. `perf stat` counts page faults (a software event; works at paranoid 2):

```sh
taskset -c 2 perf stat -e page-faults ./target/release/olap pax 7,10,11,12 200
```

```
pax [7, 10, 11, 12]: median 3.81 ms
           168,592      page-faults:u            ← ~840 per run
```

**Hypothesis:** glibc serves big allocations (the ~256–512 KB chunk buffers and column Vecs) with
`mmap` and returns them to the OS with `munmap` when freed, and trims the heap top. The next run
gets fresh pages, and each first touch of a 4 KB page is a page fault (~1 µs on this machine,
zeroing included). Inside the full `olap`, the earlier CSV phase has (most likely) already left the
allocator holding large memory, so the PAX runs reuse warm pages.

**Experiment:** tell malloc to keep memory (glibc tunables, no code change):

```sh
GLIBC_TUNABLES=glibc.malloc.mmap_threshold=67108864:glibc.malloc.trim_threshold=1073741824 \
  taskset -c 2 perf stat -e page-faults ./target/release/olap pax 7,10,11,12 200
```

```
pax [7, 10, 11, 12]: median 3.12 ms
               857      page-faults:u            ← ~4 per run
```

**Proves:** about 0.7–0.9 ms per run of fixed-only was page faults, not our code. Lesson: **the
environment is part of the measurement.** The same scan has two honest numbers ("cold allocator"
vs "warm allocator"); say which one you report.

---

## Step 5: Isolate one workload

Done: `olap pax <columns> [runs]`. perf measures the whole process, so this mode makes counters
and profiles contain only the scan (plus one `PaxFile::open`, microseconds).

---

## Step 6: `perf stat` — how much work, and what kind

```sh
taskset -c 2 perf stat -e cycles:u,instructions:u,cache-misses:u,branch-misses:u,page-faults \
    ./target/release/olap pax 7,10,11,12 100
taskset -c 2 perf stat -e cycles:u,instructions:u,cache-misses:u,branch-misses:u,page-faults \
    ./target/release/olap pax 8,9,13 100
```

**How it works.** The CPU has hardware counters (the PMU); `perf stat` reads them at start and end.
`:u` = user space only. No sampling, almost no overhead.

**Expected output (fixed):**

```
       904,644,534      cpu_core/cycles/u
     2,425,285,814      cpu_core/instructions/u
            38,495      cpu_core/cache-misses/u
            41,768      cpu_core/branch-misses/u
            84,684      page-faults:u
       0.366 seconds time elapsed
       0.224 seconds user
       0.141 seconds sys
```

**Expected output (varchar):**

```
    23,609,944,895      cpu_core/cycles/u
    95,240,575,713      cpu_core/instructions/u
        12,969,302      cpu_core/cache-misses/u
        77,407,584      cpu_core/branch-misses/u
           356,331      page-faults:u
       6.73 seconds time elapsed
       6.24 seconds user
       0.47 seconds sys
```

**Raw totals mean nothing; divide by the work** (101 runs including the warm-up):

| per run | fixed `7,10,11,12` | varchar `8,9,13` | varchar before Fix 1 |
|---|---|---|---|
| instructions | 24 M | **943 M** | 1,643 M |
| instructions / byte | 2.0 | **60** | 105 |
| instructions / Varchar value | — | **~520** | ~910 |
| IPC (instructions / cycles) | 2.7 | 4.0 | 4.5 |
| cache-misses | 0.4 K | **128 K** | 1,350 K |
| branch-misses | 0.4 K | 0.77 M | 1.1 M |
| page faults | 840 | 3,530 | 2,270 |
| kernel (`sys`) share of time | ~39% | ~7% | ~5% |

**How to read it:**

| pattern | meaning |
|---|---|
| many more instructions/byte, IPC similar or higher | the CPU does **more work** |
| similar instructions/byte, IPC much lower (< 1) | the CPU **waits** (memory stalls, pointer chasing) |
| branch-misses/row much higher | unpredictable branches in a hot loop |
| big `sys` share | time in the kernel: page faults, syscalls |

**Here:** varchar runs 30× more instructions per byte than fixed at a *high* IPC of 4. The CPU is
not waiting — it is busy doing a lot of cheap work (~520 instructions per Varchar value). That is
the signature of allocator bookkeeping, not of slow memory. Fix 1 removed ~43% of the instructions
and ~90% of the cache misses (no second copy of every String to bring through the cache).
Fixed-only spends ~39% of its time in the kernel: the page faults from 4b.

**Proves:** what *kind* of cost it is (more work, not waiting). **Does not show** where in the code.

---

## Step 6b: Stalls — is the CPU waiting, and on what?

IPC says *how much* useful work happens per cycle. Stall counters say *why* the rest of the cycles
were lost. Numbers in this step are after Fix 4 (UTF-8 checked once per chunk).

### The picture: where every cycle goes

A P-core can start up to 6 instructions per cycle ("slots"). Every slot ends up in one of four
buckets (Intel calls this **top-down analysis**):

```
            ┌───────────── front end ─────────────┐   ┌──────────── back end ────────────┐
 code ───►  fetch instructions → decode into uops  ───►  execute (ALUs, loads, stores) ──► retire
            (needs the instruction cache)              (needs data: L1 → L2 → L3 → RAM)

 Retiring        slot did useful work                         ← you want this high
 Bad speculation slot ran a wrong branch guess, thrown away
 Frontend bound  back end was ready, but no instruction arrived  (code fetch/decode too slow)
 Backend bound   instruction arrived, but had to wait            (data not in cache, or units busy)
```

The fix depends on the bucket: **Retiring-heavy → do fewer instructions** (better algorithm, less
work per value). **Backend-heavy → fix data access** (fewer passes, better locality, smaller data).
**Frontend-heavy → fix code layout** (less code in the hot loop, fewer calls). **Bad speculation →
fix unpredictable branches.**

### Start with the summary

```sh
taskset -c 2 perf stat -M TopdownL1 ./target/release/olap pax 8,9,13 100
taskset -c 2 perf stat -M TopdownL1 ./target/release/olap pax 7,10,11,12 100
```

`-M` is a *metric group*: perf picks the counters and computes the percentages. Read the
`tma_...` lines and ignore the `cpu_atom ... nan %` lines (nothing ran on an E-core).

**Expected:**

| bucket | varchar `8,9,13` | fixed `7,10,11,12` |
|---|---|---|
| `tma_retiring` | **67.6%** | 36.9% |
| `tma_frontend_bound` | 15.6% | 4.3% |
| `tma_backend_bound` | 12.9% | **57.6%** |
| `tma_bad_speculation` | ~4% | 1.2% |

**Reading it:**
- **Varchar is retiring-bound:** two thirds of the slots do useful work. The CPU is not waiting —
  it is busy running too many instructions (malloc, free, per-value loops). Stall-fixing tricks
  would gain little; **removing work** (Fix 5: no String per value) is the way.
- **Fixed width is backend-bound:** over half the slots wait for data. That matches the BOE:
  it is bandwidth-bound, copying every byte ~5 times. Here, **fewer passes over the data**
  (Fix 3, reading straight into the typed Vec) is the way.

Same tool, two workloads, two different fixes. That is what this step is for.

`-M TopdownL2` splits each bucket one level further. For varchar: frontend = 12.5% fetch
*bandwidth* (the decoder cannot keep up with many small instructions and calls) + 2.7% fetch
*latency* (waiting on the instruction cache); backend = 10.6% memory + 2.7% core.

### 1. Backend (memory) stalls

```sh
taskset -c 2 perf stat -e cycles:u,instructions:u,cycle_activity.stalls_total:u,\
cycle_activity.stalls_l1d_miss:u,cycle_activity.stalls_l3_miss:u \
    ./target/release/olap pax 8,9,13 100
```

| event | counts cycles where... |
|---|---|
| `cycle_activity.stalls_total` | the core executed nothing at all (stalled, for any reason) |
| `cycle_activity.stalls_l1d_miss` | stalled **while** a load that missed the L1 data cache was outstanding |
| `cycle_activity.stalls_l3_miss` | stalled while a load that missed L3 was outstanding: the data had to come from **RAM** |

They are nested, not separate: an L3 miss is also an L1 miss, so `l3_miss ≤ l1d_miss ≤ total`.
`total − l1d_miss` = stalls with all data in L1 (e.g. waiting on a slow instruction or a full
queue). Divide each by `cycles` to get the share of time.

**Expected (per run, 101 runs):**

| | varchar `8,9,13` | fixed `7,10,11,12` |
|---|---|---|
| cycles | 218 M | 9.8 M |
| `stalls_total` | 23.8 M = **10.9%** of cycles | 2.7 M = **27.6%** |
| `stalls_l1d_miss` | 6.2 M = 2.8% | 0.65 M = 6.7% |
| `stalls_l3_miss` (waiting on RAM) | 4.0 M = 1.8% | 0.10 M = 1.0% |

**Reading it:**
- Varchar loses only ~11% of its cycles to stalls, and under 3% to memory. **Memory is not its
  problem** (same conclusion as top-down: retiring-bound).
- Fixed width loses over a quarter of its cycles to stalls. Only ~7% are L1 misses; the rest is
  the back end saturated by the copies themselves (stores and loads queued up).

**Where the stalls happen:** sample on a stall event instead of cycles.

```sh
taskset -c 2 perf record -e cycle_activity.stalls_l3_miss:u -c 20000 -o stalls.data \
    ./target/release/olap pax 8,9,13 20
perf report -i stalls.data --no-children --stdio | grep -v '^#' | head
```

`-c 20000` = take a sample every 20,000 stall cycles. Without it these events use a period of
1,000,003 and you get only ~100–200 samples — too few to trust.

**Expected (varchar):**

```
stalls_l3_miss:   90.7%  drop_in_place<VectorData>      4.9%  Vector::take_range
stalls_l1d_miss:  62.9%  drop_in_place<VectorData>     28.9%  Vector::take_range
```

**Reading it:** almost all waiting-on-RAM happens while *dropping* Vectors. The old row group's
`Vec<String>` (65,536 × 24 B = 1.5 MB per column, 4.5 MB for three) was written by `decode` long
before; when `Scan` replaces the group, `drop` walks every one of those (now empty) String headers
to see if there is anything to free — and they are no longer in cache. `take_range` reads the same
headers as it moves them out. A small cost here (~2% of cycles), but a real finding: after Fix 1
that final walk does nothing useful, and Fix 5 removes it along with the headers.

Two cautions:
- **Skid:** a stall sample can land a few instructions after the one that actually waited, e.g.
  on the instruction after a load. Treat it as "in or right next to this function".
- **Several events in one `perf record -e a,b,c`** gives a separate table per event in
  `perf report` (`# Samples: ... of event 'cpu_core/cycles/u'`, then the next). Read each table
  for its own event; percentages are within that event, not across them.

### 2. Frontend (instruction) stalls

```sh
taskset -c 2 perf stat -e cycles:u,instructions:u,icache_data.stalls:u,icache_tag.stalls:u,\
mem_bound_stalls.ifetch:u ./target/release/olap pax 8,9,13 100
```

| event | counts cycles where... | core type |
|---|---|---|
| `icache_data.stalls` | code fetch stalled on an L1 **instruction** cache miss | P-core |
| `icache_tag.stalls` | code fetch stalled on an instruction-TLB / tag lookup miss | P-core |
| `mem_bound_stalls.ifetch` | stalled on an instruction-cache or iTLB miss served by L2/L3/RAM | **E-core only** |

Pinned to a P-core, `mem_bound_stalls.ifetch` prints `<not counted> cpu_atom/...`: the event
exists only on E-cores (`perf list --details` shows `Unit: cpu_atom`). On P-cores use the two
`icache_*` events instead.

**Expected (per run):**

| | varchar `8,9,13` | fixed `7,10,11,12` |
|---|---|---|
| `icache_data.stalls` | 0.24 M = **0.11%** of cycles | 0.03 M = 0.35% |
| `icache_tag.stalls` | 0.37 M = 0.17% | 0.05 M = 0.5% |

**Reading it:** instruction-cache stalls are tiny: the hot code (decode loop, malloc, free) fits
in the instruction cache. So the 15.6% frontend-bound from top-down is **not** cache misses — it
is fetch *bandwidth* (TopdownL2: 12.5%): the decoder cannot feed enough uops when the code is
many small instructions with frequent calls into malloc/free. Again a symptom of doing too much
per-value work, not a separate problem to fix.

**When frontend stalls do matter:** big code (many different functions in the hot path, deep
generic/trait-object call chains, huge inlined loops). Revisit this when the engine has a real
operator pipeline (Filter → Aggregate) running per chunk.

### What this step proves for us

| workload | bucket | waiting on memory? | code fetch a problem? | right kind of fix |
|---|---|---|---|---|
| varchar | retiring 68% | no (2.8% L1-miss stalls) | no (0.3%) | do less work per value (Fix 5) |
| fixed | backend 58% | some (6.7%), mostly the copies themselves | no | fewer passes over the bytes (Fix 3) |

It confirms the BOE and `perf stat` from a different angle, and it tells you **which kind of
change** can help each workload before you write any code.

---

## Step 7: Build for profiling

```sh
RUSTFLAGS="-C force-frame-pointers=yes" CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release
```

- `CARGO_PROFILE_RELEASE_DEBUG=true` adds debug info: function names, source lines, inlining.
  **Same optimizations, same speed**, bigger binary. Without it perf shows raw addresses.
- `-C force-frame-pointers=yes` keeps a pointer to the caller's frame in `rbp`, so the kernel can
  walk the stack in nanoseconds at each sample. Costs ~1–2%. Without it you need
  `--call-graph dwarf`, which on this project produced truncated stacks and a nonsense
  "100% in `f64::from_str`". **Bad stacks = wrong inclusive numbers.**

Both are environment variables; `Cargo.toml` stays unchanged. Changing `RUSTFLAGS` rebuilds
everything, and so does the next plain `cargo build`.

---

## Step 8: `perf record` — where the time goes

```sh
taskset -c 2 perf record --call-graph fp --user-callchains -F 1999 -o perf.data \
    ./target/release/olap pax 8,9,13 20
```

**Expected:** `Captured and wrote 0.5 MB perf.data (~2800 samples)`.
Without `--user-callchains` (perf 7.2.11, paranoid 2): `Failure to open any events for recording`.

**How it works.** perf programs the cycle counter to interrupt ~1999 times per second. At each
interrupt the kernel stores the current instruction, a timestamp and (with `--call-graph fp`) the
call stack. If 18% of samples land in `_int_malloc`, about 18% of CPU time was there. It is
statistics: ~3,000 samples gives about ±1–2% on the big entries. 1999 (not 2000) avoids lining up
with anything periodic.

**One raw sample** (`perf script -i perf.data | head -30`):

```
olap  122892 19786.499162:  1113541 cpu_core/cycles/Pu:
    _int_malloc                     (libc.so.6)   ← where the CPU was ("leaf")
    __libc_malloc2                  (libc.so.6)   ← its caller
    <T as ToString>::to_string      (inlined)     ← merged into its caller by the compiler
    ...Vector>::decode              (olap)        ← further up
```

- `cpu_core/cycles/Pu`: cycles on a P-core, user space.
- `(inlined)`: no real call happened; debug info still names the function. Because of inlining,
  some functions you expect (like `Scan::next`) may be missing from a stack: their code lives
  inside the caller.

`perf record` moves an existing `perf.data` to `perf.data.old` before writing, so you keep one
previous recording. Add `perf.data*` to `.gitignore`.

---

## Step 9: Profiling the full program (time windows)

Only needed when you profile plain `olap` (all phases) instead of `olap pax`. The report would mix
CSV parsing, the write and four benchmarks. Cut out the phase you want by time:

```sh
perf script -i perf.data -F time | sed -n '1p;$p'                # first and last timestamp
perf script -i perf.data -F time,sym | grep load_csv | tail -1   # last moment the CSV was parsed
perf report -i perf.data --time 4487.40,4489.42 ...              # or --time 70%-100%
```

The PAX all-columns runs are the last ~1.5 s. Timestamps change every recording. If `load_csv`
shows up in the report, the window is wrong. In hotspot: drag over the timeline, right-click →
Filter In On Selection.

---

## Step 10: Self time — where the CPU literally was

```sh
perf report -i perf.data --no-children --stdio -g none | grep -v '^#' | head -20
```

`--no-children` credits each sample only to the leaf function; self percentages add up to 100%.

```
  18.45%  olap  libc.so.6  [.] _int_malloc
  ^self   ^cmd  ^binary    ^[.] user  [k] kernel   ^function
```

**Expected (varchar `8,9,13`, after Fix 1):**

```
    18.45%  libc.so.6  [.] _int_malloc
    16.55%  olap       [.] ...Vector>::decode
    12.12%  olap       [.] core::str::converts::from_utf8
     7.39%  libc.so.6  [.] _int_free_merge_chunk
     6.66%  olap       [.] Vector::take_range
     4.77%  libc.so.6  [.] malloc
     4.32%  olap       [.] drop_in_place<VectorData>
     4.32%  libc.so.6  [.] _int_free_create_chunk
     4.18%  libc.so.6  [.] __libc_malloc2
     3.62%  libc.so.6  [.] _int_free_maybe_trim
     3.48%  libc.so.6  [.] cfree
     3.31%  libc.so.6  [.] unlink_chunk.isra.0
     3.04%  libc.so.6  [.] __memmove_avx_unaligned_erms
     2.00%  olap       [.] __rustc::__rdl_alloc
     1.95%  libc.so.6  [.] _int_free_chunk
```

**Grouped by meaning:**

| group | functions | after Fix 1 | before Fix 1 |
|---|---|---|---|
| malloc | `_int_malloc`, `malloc`, `__libc_malloc2`, `__rdl_alloc`, `__rust_alloc`, ... | ~31% | ~36% |
| free | `_int_free_*`, `cfree`, `unlink_chunk`, ... | ~24% | ~32% |
| decode loop | `Vector::decode` | ~17% | ~10% |
| UTF-8 check | `from_utf8` | ~12% | ~8% |
| take / drop bodies | `take_range`, `drop_in_place` | ~11% | — |
| String clone | `String::clone` | **gone** | ~3% self (17% inclusive) |
| copying bytes | `memmove` | ~3% | ~4% |

Shares of a smaller total: decode and `from_utf8` did not get slower, the rest shrank around them.

**Proves:** ~55% of the time is still the allocator; the byte work is under 20%.
**Does not show** which of our functions called malloc.

---

## Step 11: Inclusive time — who is responsible

```sh
perf report -i perf.data --children --stdio -g none | grep -v '^#' | grep -v inlined | head -30
```

`--children` credits a sample to every function on its stack.

**Expected (varchar `8,9,13`, after Fix 1), as a tree:**

```
pax_only closure → drain                           100%
├ Scan::next                                        73%
│  ├ PaxFile::read_row_group → read_chunk           63%
│  │  └ Vector::decode                              41%   (17% own loop, from_utf8 13%, rest malloc)
│  └ Vector::take_range                              7%   (moves headers, copies the bitmap)
└ drop_in_place<VectorData>                         25%   (each chunk's Strings, dropped in drain)
```

Before Fix 1 the same tree had `VectorData::slice` at 25% (with `String::clone` 17%) next to
decode, and `read_row_group` at 40%. Now `read_row_group` is ~2/3 of the time.

**Proves:** which of our functions own the cost: decode and its Strings, then freeing them.

---

## Step 12: Who calls malloc? — the proof

```sh
perf report -i perf.data --no-children --stdio -S _int_malloc -G | grep -E '%--' | head -12
```

`-S _int_malloc` keeps only samples in that function; `-G` prints the call tree from the top down.

**Expected (after Fix 1):**

```
18.45%  _int_malloc
 |--17.39%-- pax_only closure (drain, Scan::next, read_row_group inlined)
 |   |--12.51%-- Vec::spec_extend
 |   |     --12.23%-- <T as ToString>::to_string    ← decode: one String per row
 |    --4.88%-- GenericShunt::next                  ← read_row_group → read_chunk: the Vecs and buffers
```

**Before Fix 1** there was a second branch of the same size, `Vec::from_iter` → `String::clone`
(~10%), under `Scan::next`'s `slice(...).collect()`. **It is gone.**

And for free:

```sh
perf report -i perf.data --no-children --stdio -S _int_free_merge_chunk -G | grep -E '%--' | head -8
```

```
7.39%  _int_free_merge_chunk
 --7.06%-- pax_only closure → _int_free_chunk    ← each DataChunk's Strings, dropped in drain
```

**Proves:** almost all malloc now comes from `to_string` in `decode`; the frees are the same
Strings dropped with their chunk. That is the next thing to fix.

---

## Step 13: Annotate — the hot lines inside one function

```sh
perf report -i perf.data
```

Keys: arrows to move, **Enter** / **+** to expand, **/** to filter by name, **a** to annotate
the selected function, **h** help, **q** quit. Or open it in hotspot: `hotspot perf.data`.

Annotate shows the function's assembly interleaved with your Rust source, with the % of samples on
each instruction. In `Vector::decode` (varchar branch) the hot spots are the `from_utf8` call and
the `to_string` (call into malloc, then memcpy).

**Proves:** which source line inside a function is hot.

---

## Step 14: Check the profile against the estimate

Varchar `8,9,13`, after Fix 1, one run ≈ 66.5 ms. From the BOE section below:

- predicted ≈ 66–72 ms, of which ~40 ms is String create + free.
- Profile: allocator ~55% ≈ 37 ms. Match.
- `perf stat`: 943 M instructions per run at IPC 4.0 = 234 M cycles ≈ 62 ms at ~3.8 GHz
  (`cycles / user seconds`). Match.

They agree, so the explanation holds. **A profile you cannot reconcile with arithmetic is not
understood yet.**

---

## Step 15: Log it

`bench-results.md`, one line per measurement:

```
2026-10-23 sf0.1 warm P2  olap pax  varchar 8,9,13   101.6 ms  1,643 M instr  baseline
2026-10-23 sf0.1 warm P2  olap pax  all 0..15        200.5 ms                 baseline
2026-10-24 sf0.1 warm P2  olap pax  varchar 8,9,13    66.5 ms    943 M instr  Fix 1 take_range  (-35%)
2026-10-24 sf0.1 warm P2  olap pax  all 0..15        143.4 ms                 Fix 1 take_range  (-28%)
2026-10-24 sf0.1 warm P2  olap pax  fixed 7,10,11,12   3.6 ms     24 M instr  Fix 1 (unchanged, as predicted)
2026-10-24 sf0.1 warm P2  olap pax  varchar 8,9,13    62.3 ms    956 M instr  Fix 4 utf8 once  (-4.2 ms; predicted -5..-8)
2026-10-24 sf0.1 warm P2  olap pax  all 0..15        122.7 ms                 Fix 4 utf8 once  (-20.7 ms)
```

**Fix 4 compared with its prediction:**
- **Varchar `8,9,13` saved a little less than predicted** (4.2 vs 5–8 ms), and the instruction
  count went slightly *up* (943 → 956 M). The 1.8 M `get` boundary checks plus the one big pass
  cost more than the ~2 ms estimated. Two of these columns are 1-byte values, for which the old
  per-value check was already cheap.
- **All columns saved more than the varchar estimate suggests** (20.7 ms): `l_comment` (~27 B)
  and `l_shipmode` / `l_shipinstruct` are longer, and the old per-value check cost grew with the
  length. A per-value cost is not a constant; it depends on the value size. The BOE improves
  by measuring `from_utf8` at the real lengths.

After each change, repeat Steps 4, 6, 8, 10 and 12 and compare against the log.

---

# Back-of-envelope: predict before you profile

A back-of-envelope (BOE) estimate says what a number *should* be, from two things you can know
without a profiler: **how much work the code does** (counted from the code) and **what each unit
of work costs** (measured once on this machine). Then:

- **measured ≈ predicted** (within ~±30%): you understand where the time goes. Profile only to
  confirm, or to find the next fix.
- **measured ≫ predicted**: something costs more than you think. That gap *is* the question to
  profile. (Page faults in 4b were found this way.)
- **measured ≪ predicted**: you counted something that does not happen (the compiler removed it,
  or it was cached). Also worth understanding.
- **predicted floor ≪ measured**: there is room to improve, and the gap says how much.

## BOE step 1: Measure this machine's unit costs

```sh
cargo build --release --example calib
taskset -c 2 target/release/examples/calib
```

**Expected:**

```
memcpy    256 KB:                52.4 GB/s       ← fits in L2
memcpy   4096 KB:                20.6 GB/s       ← L3
memcpy  65536 KB:                18.3 GB/s       ← RAM
decode i64 (8 MB):                36.0 GB/s of input
String create + free:             22.1 ns each
String create + clone + 2 frees:  44.6 ns per value
from_utf8 per 12 B value:          8.5 ns each
from_utf8 over 12 MB at once:     37.1 GB/s
page fault (first touch, 4 KB):   1061 ns each
```

**Why measure instead of using textbook numbers:** they vary 2–3× between machines, allocators
and kernels. Measured here, they make predictions you can check to ±20%.

**The unit costs for this machine:**

| unit | cost | where it shows up in a scan |
|---|---|---|
| copy, small working set (L2) | ~50 GB/s | take_range of a 2048-row piece |
| copy, medium (L3) | ~20 GB/s | decode of a 0.5–1.5 MB chunk |
| copy, from RAM | ~18 GB/s | `pread` from the page cache |
| decode fixed width | ~36 GB/s of input | `Vector::decode`, Int64/Float64/Date |
| one String create + free | ~22 ns | `to_string` in decode + drop in drain |
| one String clone + free | ~23 ns | the old `Vector::slice` (before Fix 1) |
| `from_utf8` per small value | ~5–8.5 ns | decode, per Varchar row |
| `from_utf8` over a buffer | ~37 GB/s | (if done once per chunk) |
| page fault | ~1 µs per 4 KB | first touch of a fresh large allocation |

## BOE step 2: Count the work from the code

For one run, count:

- **bytes × passes**: how many times each byte is read or written, and in which cache level.
- **per-value operations**: anything done once per row (String create, UTF-8 check).
- **per-call costs**: syscalls (`pread`: 1 per chunk), allocations of big buffers.
- **page faults**: from `perf stat -e page-faults` (count once; it is cheap to measure).

**Sizes for SF0.1 lineitem:** 600,572 rows; 10 row groups (9 × 65,536 + 10,748); 294 DataChunks
per scan. String header = 24 B. On disk: Int64/Float64 8 B, Date 4 B, Varchar 4 B offset + bytes.

## BOE step 3: Worked example — fixed width `7,10,11,12` (12.0 MB)

Follow one byte from the page cache to the sum:

| pass | bytes | rate | time |
|---|---|---|---|
| `vec![0u8; n]` zeroes the read buffer | 12 MB written | ~30 GB/s | 0.40 ms |
| `pread`: kernel copies page cache → buffer | 12 MB | ~18 GB/s (RAM) | 0.67 ms |
| `decode` → typed Vecs | 12 MB | ~36 GB/s | 0.33 ms |
| `take_range` copies each 2048-row piece | 12 MB | ~20–50 GB/s | 0.25–0.6 ms |
| page faults (~840 measured) | 840 × 1.06 µs | | 0.9 ms |
| **total** | | | **2.6–2.9 ms** |

**Measured:** 3.6 ms with `olap pax` (cold allocator), 2.7 ms inside `olap` (warm allocator, few
faults → predicted 1.7–2.0 ms).

**Reading it:** within ~30%, so no big surprise. Cross-check with `perf stat`: `sys` = 1.4 ms per
run ≈ faults 0.9 + kernel copy 0.67 = 1.57 ms. **Match.** User time = 2.2 ms vs ~1.0–1.3 ms
predicted for the user-space passes: **~1 ms unexplained.** Candidates: the cache misses on
fresh pages after every `munmap`, and the many large allocations themselves. Fix 3 (reuse the read
buffer) is the experiment that would tell.

**Floor ("speed of light"):** the unavoidable work is one copy out of the page cache:
12 MB / 18 GB/s ≈ **0.7 ms**. Now 3.6 ms → about **5× headroom**, all in extra passes and faults.

## BOE step 4: Worked example — Varchar `8,9,13` (15.6 MB, 1.80 M values)

Values: 3 columns × 600,572 rows = 1.80 M. Lengths: `l_returnflag` 1, `l_linestatus` 1,
`l_shipinstruct` ~12 bytes.

**Before Fix 1:**

| cost | count | unit | time |
|---|---|---|---|
| String create + clone + 2 frees | 1.80 M | 44.6 ns | 80 ms |
| `from_utf8` per value (short values) | 1.80 M | 5–8.5 ns | 9–15 ms |
| String headers: decode writes, slice reads + writes, 2 drops read | 5 × 43 MB | ~20 GB/s | ~10 ms |
| offsets `Vec<usize>`: write + read | 2 × 14.4 MB | ~20 GB/s | ~1.5 ms |
| read buffer: zero + kernel copy | 3 × 15.6 MB | ~20 GB/s | ~2.5 ms |
| page faults (~2,270 measured) | | 1.06 µs | 2.4 ms |
| **total** | | | **105–111 ms** |

**Measured:** 101.6 ms. Within 10%.

**After Fix 1** (the clone and one free per value are gone):

| cost | count | unit | time |
|---|---|---|---|
| String create + free | 1.80 M | 22.1 ns | 40 ms |
| `from_utf8` per value | 1.80 M | 5–8.5 ns | 9–15 ms |
| String headers: decode writes, take reads + writes, 2 drops read | 5 × 43 MB | ~20 GB/s | ~10 ms |
| offsets, read buffer | | | ~4 ms |
| page faults (~3,530 measured) | | 1.06 µs | 3.7 ms |
| **total** | | | **67–73 ms** |

**Measured:** 66.5 ms. Within 10%.

**Predicted gain of Fix 1:** 1.80 M × (44.6 − 22.1) ns ≈ **40 ms**. **Measured gain: 35 ms.**
That is how you know the fix did what you thought — not just "it got faster".

**Floor:** with Varchar as one byte buffer + offsets (Fix 5): kernel copy 15.6 MB (0.9 ms) +
one `from_utf8` over the data (0.4 ms) + copying offsets and bytes (~0.5 ms) ≈ **2 ms**.
Now 66.5 ms → about **30× headroom**, almost all in per-String work.

## BOE step 5: Predict the next fixes (varchar `8,9,13`, from 66.5 ms)

| fix | removes | predicted saving | predicted result |
|---|---|---|---|
| 2: read offsets in place | 2 × 14.4 MB + their faults | ~1.5–4 ms | ~63–65 ms |
| 3: reuse one read buffer | zeroing + mmap faults | ~1–3 ms (fixed width: ~1 ms of 3.6) | ~64–65 ms |
| 4: `from_utf8` once per chunk | 1.80 M checks → one pass | ~8–14 ms (profile: `from_utf8` 13% ≈ 8.7 ms) | ~53–58 ms |
| 5: Varchar as bytes + offsets | String create + free, header traffic, per-value UTF-8 | ~55–60 ms | **~5–10 ms** |

Write the prediction in the log *before* each fix, then measure. A fix that saves much less than
predicted means a cost you have not counted; one that saves more means you counted something
twice. Either way, the BOE gets better.

## BOE checklist

1. Measure unit costs once per machine (`calib`), and again after OS or kernel updates.
2. Count bytes × passes, per-value operations, syscalls, page faults for one run.
3. Multiply, add, compare with the measured median.
4. Within ~30%: understood. Far off: that gap is what to profile.
5. Compute the floor: the work that cannot be avoided. Floor vs measured = headroom.
6. Before each fix: predict the saving. After: compare. Log both.

---

## Cheat sheet

```sh
# measure
cargo build --release
taskset -c 2 ./target/release/olap                      # everything (creates lineitem.pax)
taskset -c 2 ./target/release/olap pax 8,9,13 [runs]    # one workload

# unit costs for BOE
cargo build --release --example calib && taskset -c 2 target/release/examples/calib

# counters (how much, what kind)
taskset -c 2 perf stat -e cycles:u,instructions:u,cache-misses:u,branch-misses:u,page-faults \
    ./target/release/olap pax 8,9,13 100

# stalls: which bucket, then backend (memory) and frontend (instruction fetch)
taskset -c 2 perf stat -M TopdownL1 ./target/release/olap pax 8,9,13 100        # or TopdownL2
taskset -c 2 perf stat -e cycles:u,instructions:u,cycle_activity.stalls_total:u,\
cycle_activity.stalls_l1d_miss:u,cycle_activity.stalls_l3_miss:u ./target/release/olap pax 8,9,13 100
taskset -c 2 perf stat -e cycles:u,instructions:u,icache_data.stalls:u,icache_tag.stalls:u \
    ./target/release/olap pax 8,9,13 100
taskset -c 2 perf record -e cycle_activity.stalls_l3_miss:u -c 20000 -o stalls.data \
    ./target/release/olap pax 8,9,13 20                                         # where it waits

# profile (where)
RUSTFLAGS="-C force-frame-pointers=yes" CARGO_PROFILE_RELEASE_DEBUG=true cargo build --release
taskset -c 2 perf record --call-graph fp --user-callchains -F 1999 -o perf.data \
    ./target/release/olap pax 8,9,13 20
perf report -i perf.data --no-children --stdio -g none                # self
perf report -i perf.data --children    --stdio -g none                # inclusive
perf report -i perf.data --no-children --stdio -S _int_malloc -G      # who calls malloc
perf report -i perf.data                                              # interactive, 'a' = annotate
hotspot perf.data                                                     # GUI, flame graph

# environment experiments
GLIBC_TUNABLES=glibc.malloc.mmap_threshold=67108864:glibc.malloc.trim_threshold=1073741824 ...
sync; echo 3 | sudo tee /proc/sys/vm/drop_caches                      # cold page cache
```

| tool | answers | does not answer |
|---|---|---|
| `bench::measure` | how long | why |
| BOE + `calib` | how long it should be; the floor | where the gap is |
| experiment (Step 4) | which input causes the cost | which code |
| `perf stat` | more work or waiting; kernel share | where |
| top-down / stall counters (Step 6b) | which bucket: useful work, memory, code fetch, branches | which line |
| `perf record` self | which function the CPU was in | who called it |
| `perf record` children / `-G` | which of our functions own the cost | why that design is slow |
| annotate | which line / instruction | — |
