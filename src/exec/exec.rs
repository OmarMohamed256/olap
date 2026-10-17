use crate::catalog::DataType;

pub const VECTOR_SIZE: usize = 2048;
// (one typed column of values plus a null bitmap)
pub struct Bitmap {
    words: [u64; 32], // 32 × 64 = 2048 bits
}
impl Bitmap {
    pub fn all_valid() -> Self {
        Bitmap {
            words: [u64::MAX; 32],
        } // every bit = 1 → no NULLs
    }
    pub fn is_valid(&self, i: usize) -> bool {
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }
    pub fn set_null(&mut self, i: usize) {
        self.words[i / 64] &= !(1 << (i % 64));
    }
}
pub enum VectorData {
    Int64(Vec<i64>), // 2048 × 8 bytes, packed
    Float64(Vec<f64>),
    Date(Vec<i32>), // days since 1970-01-01
    Varchar(Vec<String>),
}

impl VectorData {
    pub fn new(ty: DataType) -> Self {
        match ty {
            DataType::Int64 => VectorData::Int64(Vec::new()),
            DataType::Float64 => VectorData::Float64(Vec::new()),
            DataType::Date => VectorData::Date(Vec::new()),
            DataType::Varchar => VectorData::Varchar(Vec::new()),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            VectorData::Int64(v) => v.len(),
            VectorData::Float64(v) => v.len(),
            VectorData::Date(v) => v.len(),
            VectorData::Varchar(v) => v.len(),
        }
    }

    /// Copies `count` values starting at `start` into a new VectorData.
    pub fn slice(&self, start: usize, count: usize) -> Self {
        let range = start..start + count;
        match self {
            VectorData::Int64(v) => VectorData::Int64(v[range].to_vec()),
            VectorData::Float64(v) => VectorData::Float64(v[range].to_vec()),
            VectorData::Date(v) => VectorData::Date(v[range].to_vec()),
            VectorData::Varchar(v) => VectorData::Varchar(v[range].to_vec()),
        }
    }

    pub fn format(&self, row: usize) -> String {
        match self {
            VectorData::Int64(v) => v[row].to_string(),
            VectorData::Float64(v) => v[row].to_string(),
            VectorData::Date(v) => format_date(v[row]),
            VectorData::Varchar(v) => v[row].clone(),
        }
    }
}

/// 9568 -> "1996-03-13". From Howard Hinnant's
/// "chrono-Compatible Low-Level Date Algorithms".
fn format_date(days: i32) -> String {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}
pub struct Vector {
    pub data: VectorData, // up to 2048 values of one type
    pub validity: Bitmap, // 2048 bits: which of those values are NULL
}
pub struct DataChunk {
    pub columns: Vec<Vector>, // one Vector per column, e.g. 16 for SELECT * FROM lineitem
    pub count: usize,         // how many rows are filled in (≤ 2048)
}

pub trait Operator {
    fn next(&mut self) -> Option<DataChunk>;
}

pub trait TableSource {
    fn num_rows(&self) -> usize;
    /// Read `count` values of one column, starting at row `start`.
    fn read(&self, column: usize, start: usize, count: usize) -> Vector;
}

pub struct Scan {
    source: Box<dyn TableSource>,
    columns: Vec<usize>, // which columns to read, e.g. vec![4, 6] for l_quantity, l_discount
    position: usize,     // next row to read
}

impl Scan {
    pub fn new(source: Box<dyn TableSource>, columns: Vec<usize>) -> Self {
        Scan {
            source,
            columns,
            position: 0,
        }
    }
}

// Scan(Table) : Dataflow
// The Scan operator retrieves data vector-at-a-time from
impl Operator for Scan {
    fn next(&mut self) -> Option<DataChunk> {
        let remaining = self.source.num_rows() - self.position;
        if remaining == 0 {
            return None; // done
        }
        let count = remaining.min(VECTOR_SIZE); // the last chunk can be smaller

        let columns = self
            .columns
            .iter()
            .map(|&col| self.source.read(col, self.position, count))
            .collect();

        self.position += count;
        Some(DataChunk { columns, count })
    }
}
