#!/usr/bin/env bash
# Generate TPC-H test data and DuckDB's expected answers (the oracle).
#
# Usage: scripts/gen_tpch.sh [SF ...]     (default: 0.1 1)
#
# Produces, relative to the repo root:
#   data/sf<SF>/tpch.duckdb        DuckDB database holding the generated tables
#   data/sf<SF>/<table>.csv        '|'-delimited CSV with header
#   data/sf<SF>/<table>.parquet    Parquet copy (for format comparisons)
#   queries/q<N>.sql               query text for each target query
#   expected/sf<SF>/q<N>.csv       DuckDB's result for each target query
#
# Re-running regenerates everything for the given scale factors.
set -euo pipefail

command -v duckdb >/dev/null || { echo "duckdb CLI not found in PATH" >&2; exit 1; }

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ $# -gt 0 ]]; then SCALE_FACTORS=("$@"); else SCALE_FACTORS=(0.1 1); fi
QUERIES=(1 3 4 5 6 17)
TABLES=(lineitem orders customer part partsupp supplier nation region)

mkdir -p queries
for q in "${QUERIES[@]}"; do
  duckdb -list -noheader -c "LOAD tpch; SELECT query FROM tpch_queries() WHERE query_nr = $q" \
    > "queries/q$q.sql"
done
echo "wrote query text for: ${QUERIES[*]}"

for sf in "${SCALE_FACTORS[@]}"; do
  out="data/sf$sf"
  exp="expected/sf$sf"
  db="$out/tpch.duckdb"
  mkdir -p "$out" "$exp"
  rm -f "$db" "$db.wal"

  echo "== SF $sf: generating data"
  export_sql="INSTALL tpch; LOAD tpch; CALL dbgen(sf = $sf);"
  for t in "${TABLES[@]}"; do
    export_sql+=" COPY $t TO '$out/$t.csv' (FORMAT csv, HEADER, DELIMITER '|');"
    export_sql+=" COPY $t TO '$out/$t.parquet' (FORMAT parquet);"
  done
  duckdb "$db" -c "$export_sql"

  echo "== SF $sf: saving expected answers"
  for q in "${QUERIES[@]}"; do
    duckdb "$db" -readonly -csv < "queries/q$q.sql" > "$exp/q$q.csv"
    echo "   q$q: $(($(wc -l < "$exp/q$q.csv") - 1)) rows"
  done
done

echo "done."
