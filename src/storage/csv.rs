use crate::catalog::TableSchema;
use crate::exec::exec::{Bitmap, TableSource, Vector, VectorData};
use std::fs::File;
use std::io::{BufRead, BufReader};

/// Parses `field` according to the column's type and appends it.
fn push_field(column: &mut VectorData, field: &str) {
    match column {
        VectorData::Int64(v) => v.push(field.parse().unwrap()),
        VectorData::Float64(v) => v.push(field.parse().unwrap()),
        VectorData::Date(v) => v.push(parse_date(field)),
        VectorData::Varchar(v) => v.push(field),
    }
}

/// Loads a '|'-delimited CSV (with a header row) into one array per column.
pub fn load_csv(path: &str, schema: &TableSchema) -> Vec<VectorData> {
    let mut columns: Vec<VectorData> = schema
        .columns
        .iter()
        .map(|c| VectorData::new(c.ty))
        .collect();

    let reader = BufReader::new(File::open(path).unwrap());
    let mut lines = reader.lines();

    // The header must list the catalog's columns in the same order,
    // otherwise values would land in the wrong column.
    let header = lines.next().unwrap().unwrap();
    let names: Vec<&str> = header.split('|').collect();
    let expected: Vec<&str> = schema.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names, expected,
        "{path}: CSV header does not match the catalog"
    );

    for line in lines {
        let line = line.unwrap();
        for (column, field) in columns.iter_mut().zip(line.split('|')) {
            push_field(column, field);
        }
    }

    columns
}

/// "1996-03-13" -> 9568 (days since 1970-01-01). From Howard Hinnant's
/// "chrono-Compatible Low-Level Date Algorithms".
fn parse_date(s: &str) -> i32 {
    let y: i32 = s[0..4].parse().unwrap();
    let m: i32 = s[5..7].parse().unwrap();
    let d: i32 = s[8..10].parse().unwrap();

    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub struct CsvTable {
    columns: Vec<VectorData>,
}

impl CsvTable {
    pub fn load(path: &str, schema: &TableSchema) -> Self {
        CsvTable {
            columns: load_csv(path, schema),
        }
    }
}

impl TableSource for CsvTable {
    fn num_row_groups(&self) -> usize {
        1
    }

    fn row_group_num_rows(&self, _rg: usize) -> usize {
        // first() is None for a table with no columns, which has no rows
        self.columns.first().map_or(0, |column| column.len())
    }

    fn read_row_group(&self, rg: usize, columns: &[usize]) -> Result<Vec<Vector>, String> {
        if rg != 0 {
            return Err(format!("CSV table has one row group, asked for {rg}"));
        }
        let num_rows = self.row_group_num_rows(rg);
        // Copies each column: the table keeps its own data and Scan needs owned Vectors.
        // CSV has no NULLs, so every row is valid.
        columns
            .iter()
            .map(|&column| {
                let data = self
                    .columns
                    .get(column)
                    .ok_or(format!("column {column} out of range"))?
                    .clone();
                Ok(Vector {
                    data,
                    validity: Bitmap::all_valid(num_rows),
                })
            })
            .collect()
    }
}
