mod bench;
mod catalog;
mod exec;
mod storage;

use std::collections::HashMap;
use std::fs;
use std::hint::black_box;
use std::ops::Bound;
use std::time::Instant;

use sqlparser::ast::{LimitClause, SelectItem, SetExpr, Statement, TableFactor};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::catalog::Catalog;
use crate::exec::exec::{
    Bitmap, ColumnRange, Operator, Scalar, Scan, TableSource, Vector, VectorData,
};
use crate::storage::csv::{CsvTable, load_csv};
use crate::storage::pax::{self, PaxFile};

const DATA_DIR: &str = "data/sf0.1";

fn main() {
    // CSV -> PAX: load lineitem, write it with our own writer, compare sizes.
    let catalog = Catalog::tpch();
    let schema = catalog.get("lineitem").unwrap();
    let csv_path = format!("{DATA_DIR}/lineitem.csv");
    let pax_path = format!("{DATA_DIR}/lineitem.pax");

    // `olap pax 8,9 [runs]`: only scan those columns of the existing PAX file, so perf
    // measures one workload instead of the whole program (see docs/profiling.md).
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "pax" {
        if let Err(e) = pax_only(&pax_path, schema.columns.len(), &args[2], args.get(3)) {
            println!("error: {e}");
        }
        return;
    }

    let start = Instant::now();
    // load_csv gives values only; CSV has no NULLs, so every bitmap is all-valid.
    let columns: Vec<Vector> = load_csv(&csv_path, schema)
        .into_iter()
        .map(|data| {
            let validity = Bitmap::all_valid(data.len());
            Vector { data, validity }
        })
        .collect();
    println!(
        "load csv:  {:?} ({} rows)",
        start.elapsed(),
        columns[0].data.len()
    );

    let start = Instant::now();
    if let Err(e) = pax::write(&pax_path, schema, &columns) {
        println!("write failed: {e}");
        return;
    }
    println!("write pax: {:?}", start.elapsed());

    let parquet_path = format!("{DATA_DIR}/lineitem.parquet");
    for path in [&csv_path, &parquet_path, &pax_path] {
        let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        println!("{path}: {:.1} MB", bytes as f64 / 1e6);
    }

    // Times scanning the same columns from CSV and PAX, from disk. Correctness of the data is
    // checked by tests (pax.rs scan_projects_columns); here only the row counts must agree.
    // CSV has to be parsed on every query, so its time includes load_csv; PAX includes open.
    let rows = columns[0].data.len();
    let csv_bytes = fs::metadata(&csv_path).map(|m| m.len()).unwrap_or(0);
    let pax_file = PaxFile::open(&pax_path).unwrap();
    let q6 = vec![4, 5, 6, 10]; // l_quantity, l_extendedprice, l_discount, l_shipdate
    let all: Vec<usize> = (0..schema.columns.len()).collect();

    for (label, scan_columns) in [
        ("Q6 columns", &q6),
        ("fixed only", &vec![7, 10, 11, 12]),
        ("varchar only", &vec![8, 9, 13]),
        ("all columns", &all),
    ] {
        let pax_bytes = pax_file.columns_bytes(scan_columns);

        let csv = bench::measure(&format!("csv {label}"), 3, rows, csv_bytes, || {
            let source = CsvTable::load(&csv_path, schema);
            drain(Scan::new(
                Box::new(source),
                scan_columns.clone(),
                Vec::new(),
            ))
            .unwrap()
        });
        let pax = bench::measure(&format!("pax {label}"), 10, rows, pax_bytes, || {
            let source = PaxFile::open(&pax_path).unwrap();
            drain(Scan::new(
                Box::new(source),
                scan_columns.clone(),
                Vec::new(),
            ))
            .unwrap()
        });
        assert_eq!(
            csv, pax,
            "{label}: CSV and PAX scanned different row counts"
        );
    }

    // Getting a feel of skipped groups for filter push down
    let filter = vec![ColumnRange {
        column: 0,
        low: Bound::Included(Scalar::I64(1)),
        high: Bound::Included(Scalar::I64(3)),
    }];
    let mut scan = Scan::new(Box::new(PaxFile::open(&pax_path).unwrap()), vec![0], filter);
    let mut rows = 0;
    while let Some(chunk) = scan.next().unwrap() {
        rows += chunk.count;
    }
    println!(
        "pushdown l_orderkey 1..=3: {rows} rows read, {} groups skipped",
        scan.skipped_groups()
    );
}

// Scans `columns` (e.g. "8,9") of the PAX file `runs` times (default 20) and prints the timing.
// Run `olap` without arguments first, so the PAX file exists.
fn pax_only(
    path: &str,
    num_columns: usize,
    columns: &str,
    runs: Option<&String>,
) -> Result<(), String> {
    let columns: Vec<usize> = columns
        .split(',')
        .map(|c| c.trim().parse().map_err(|_| format!("bad column: {c}")))
        .collect::<Result<_, _>>()?;
    let runs: usize = match runs {
        Some(r) => r.parse().map_err(|_| format!("bad runs: {r}"))?,
        None => 20,
    };
    if let Some(c) = columns.iter().find(|&&c| c >= num_columns) {
        return Err(format!(
            "column {c} out of range: the table has {num_columns}"
        ));
    }

    let file = PaxFile::open(path)?;
    let rows = (0..file.num_row_groups())
        .map(|g| file.row_group_num_rows(g))
        .sum();
    let bytes = file.columns_bytes(&columns);

    let scanned = bench::measure(&format!("pax {columns:?}"), runs, rows, bytes, || {
        let source = PaxFile::open(path).unwrap();
        drain(Scan::new(Box::new(source), columns.clone(), Vec::new())).unwrap()
    });
    println!(
        "  {scanned} rows, {:.1} MB scanned per run",
        bytes as f64 / 1e6
    );
    Ok(())
}

// Pulls every chunk out of a Scan and returns the row count. black_box stops the compiler
// from skipping work whose result is unused.
fn drain(mut scan: Scan) -> Result<usize, String> {
    let mut rows = 0;
    // FREE: each chunk is dropped at the end of the loop body: its Vecs (Varchar: two buffers
    // per column, offsets and bytes).
    while let Some(chunk) = scan.next()? {
        rows += black_box(&chunk).count;
    }
    Ok(rows)
}

/// Runs `SELECT * FROM <table> [LIMIT n]` and prints the result.
fn run(
    sql: &str,
    catalog: &Catalog,
    tables: &mut HashMap<String, Vec<VectorData>>,
) -> Result<(), String> {
    let (table_name, limit) = parse_select(sql)?;
    let schema = catalog
        .get(&table_name)
        .ok_or(format!("table not found: {table_name}"))?;

    let columns = tables
        .entry(table_name.clone())
        .or_insert_with(|| load_csv(&format!("{DATA_DIR}/{table_name}.csv"), schema));

    let num_rows = columns[0].len();
    let limit = limit.unwrap_or(num_rows).min(num_rows);

    let names: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    println!("{}", names.join(" | "));
    for row in 0..limit {
        let values: Vec<String> = columns.iter().map(|c| c.format(row)).collect();
        println!("{}", values.join(" | "));
    }
    println!("({limit} rows)");
    Ok(())
}

/// Extracts the table name and optional LIMIT from `SELECT * FROM <table> [LIMIT n]`.
fn parse_select(sql: &str) -> Result<(String, Option<usize>), String> {
    let statements = Parser::parse_sql(&GenericDialect {}, sql).map_err(|e| e.to_string())?;

    let [Statement::Query(query)] = statements.as_slice() else {
        return Err("expected a single SELECT statement".into());
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Err("expected a SELECT".into());
    };
    let [SelectItem::Wildcard(_)] = select.projection.as_slice() else {
        return Err("only SELECT * is supported".into());
    };
    let [from] = select.from.as_slice() else {
        return Err("expected exactly one table in FROM".into());
    };
    let TableFactor::Table { name, .. } = &from.relation else {
        return Err("expected a table name in FROM".into());
    };

    let limit = match &query.limit_clause {
        Some(LimitClause::LimitOffset {
            limit: Some(expr), ..
        }) => Some(
            expr.to_string()
                .parse()
                .map_err(|_| format!("bad LIMIT: {expr}"))?,
        ),
        _ => None,
    };

    Ok((name.to_string().to_lowercase(), limit))
}
