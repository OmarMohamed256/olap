mod catalog;
mod exec;
mod storage;

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use sqlparser::ast::{LimitClause, SelectItem, SetExpr, Statement, TableFactor};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::catalog::Catalog;
use crate::exec::exec::{Bitmap, Vector, VectorData};
use crate::storage::csv::load_csv;
use crate::storage::pax;

const DATA_DIR: &str = "data/sf0.1";

fn main() {
    //     let catalog = Catalog::tpch();
    //     // Tables are loaded from CSV the first time a query uses them.
    //     let mut tables: HashMap<String, Vec<VectorData>> = HashMap::new();
    //
    //     loop {
    //         print!("> ");
    //         io::stdout().flush().unwrap();
    //
    //         let mut line = String::new();
    //         if io::stdin().read_line(&mut line).unwrap() == 0 {
    //             break; // Ctrl-D
    //         }
    //         let sql = line.trim();
    //         if sql.is_empty() {
    //             continue;
    //         }
    //         if sql == "exit" || sql == "quit" {
    //             break;
    //         }
    //
    //         if let Err(e) = run(sql, &catalog, &mut tables) {
    //             println!("error: {e}");
    //         }
    //     }
    //

    // CSV -> PAX: load lineitem, write it with our own writer, compare sizes.
    let catalog = Catalog::tpch();
    let schema = catalog.get("lineitem").unwrap();
    let csv_path = format!("{DATA_DIR}/lineitem.csv");
    let pax_path = format!("{DATA_DIR}/lineitem.pax");

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

    for path in [csv_path, format!("{DATA_DIR}/lineitem.parquet"), pax_path] {
        let bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        println!("{path}: {:.1} MB", bytes as f64 / 1e6);
    }
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
