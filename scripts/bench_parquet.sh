#!/usr/bin/env bash
# Time DuckDB scanning the Parquet copy of lineitem, to compare with the PAX scan in `olap`.
#
# Usage: scripts/bench_parquet.sh [SF] [RUNS]     (default: 0.1 10)
#
# One thread, warm cache (one untimed run first), rows discarded (.mode trash).
# Prints min and median of RUNS runs for each column set, then EXPLAIN ANALYZE's total
# for a full scan, which leaves out handing rows to the CLI (closest to olap's drain).
set -euo pipefail

command -v duckdb >/dev/null || { echo "duckdb CLI not found in PATH" >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SF="${1:-0.1}"
RUNS="${2:-10}"
FILE="data/sf$SF/lineitem.parquet"
[[ -f "$FILE" ]] || { echo "$FILE not found: run scripts/gen_tpch.sh $SF first" >&2; exit 1; }

# Same column sets as main.rs.
declare -A QUERIES=(
  ["Q6 columns"]="l_quantity, l_extendedprice, l_discount, l_shipdate"
  ["fixed only"]="l_tax, l_shipdate, l_commitdate, l_receiptdate"
  ["varchar only"]="l_returnflag, l_linestatus, l_shipinstruct"
  ["all columns"]="*"
)

for label in "Q6 columns" "fixed only" "varchar only" "all columns"; do
  select="SELECT ${QUERIES[$label]} FROM '$FILE';"
  # Warm-up run, then RUNS timed runs; keep only the "real" seconds of each.
  times=$(
    { echo "SET threads = 1;"; echo ".mode trash"; echo "$select"
      echo ".timer on"; for _ in $(seq "$RUNS"); do echo "$select"; done; } |
      duckdb | awk '/^Run Time/ { print $5 * 1000 }' | sort -n
  )
  echo "$times" | awk -v label="$label" -v n="$RUNS" '
    { t[NR] = $1 }
    END { printf "parquet %s: min %.2fms, median %.2fms\n", label, t[1], t[int((n + 1) / 2)] }'
done

total=$(
  printf "SET threads = 1;\nEXPLAIN ANALYZE SELECT * FROM '%s';\n" "$FILE" |
    duckdb | grep -o 'Total Time: [0-9.]*s' | grep -o '[0-9.]*'
)
echo "parquet all columns, EXPLAIN ANALYZE total: $(awk -v s="$total" 'BEGIN { printf "%.2f", s * 1000 }')ms"
