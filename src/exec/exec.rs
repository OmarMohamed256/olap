use crate::catalog::DataType;

pub const VECTOR_SIZE: usize = 2048;
// Validity of a Vector: bit i = 1 means value i is present, 0 means NULL.
#[derive(Debug, PartialEq)]
pub struct Bitmap {
    pub words: Vec<u64>, // one bit per row, 64 rows per word: 2048 rows → 32 words, 65,536 → 1024
}
impl Bitmap {
    // Words needed for `num_rows` bits. Shared by all_valid and the PAX decoder so they always agree.
    pub fn num_words(num_rows: usize) -> usize {
        (num_rows + 63) / 64 // round up: 100 rows still need 2 words
    }
    pub fn all_valid(num_rows: usize) -> Self {
        Bitmap {
            words: vec![u64::MAX; Bitmap::num_words(num_rows)],
        } // every bit = 1 → no NULLs
    }
    pub fn is_valid(&self, i: usize) -> bool {
        // i / 64 --> i >> 6  (Shift right by 6 is identical to dividing by 2^6)
        // i % 64 --> i & 63  (Masking with 0b111111 isolates the remainder of division by 64)
        // Equivalent optimized hardware execution:
        // (self.words[i >> 6] >> (i & 63)) & 1 == 1
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }
    // NULLs among the first `num_rows` bits; padding bits past num_rows are ignored.
    pub fn null_count(&self, num_rows: usize) -> usize {
        self.null_count_range(0, num_rows)
    }
    // NULLs among rows start..start + num_rows. `start` must be a multiple of 64 so the
    // range begins on a word boundary (row groups always do: 65,536 = 1024 × 64).
    pub fn null_count_range(&self, start: usize, num_rows: usize) -> usize {
        assert!(
            start % 64 == 0,
            "null_count_range: start {start} is not a multiple of 64"
        );
        let first_word = start / 64;
        let mut num_words = Bitmap::num_words(num_rows);
        // More rows than bits is a caller bug: fail with a clear message, not an index error.
        assert!(
            first_word + num_words <= self.words.len(),
            "null_count: {num_rows} rows from row {start} but bitmap has only {} words",
            self.words.len()
        );
        let words = &self.words[first_word..first_word + num_words]; // just this range's words, no copy
        let mut total_valid = 0;

        // check if there is leftover rows
        if num_rows & 63 > 0 {
            num_words -= 1;
            let mask: u64 = (1u64 << (num_rows & 63)) - 1;
            total_valid += (words[num_words] & mask).count_ones();
        }
        for i in 0..num_words {
            total_valid += words[i].count_ones();
        }
        num_rows - total_valid as usize
    }
    pub fn set_null(&mut self, i: usize) {
        // COMPILER TRANSLATION:
        // i / 64 --> i >> 6
        // i % 64 --> i & 63
        //
        // Equivalent optimized hardware execution:
        // self.words[i >> 6] &= !(1 << (i & 63));
        self.words[i / 64] &= !(1 << (i % 64));
    }
}
// The values of one column, without validity (that lives in Vector.validity).
// NULL rows still take a slot so value i always belongs to row i.
#[derive(Debug, PartialEq)]
pub enum VectorData {
    Int64(Vec<i64>), // 8 bytes per value, packed
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
    // The catalog type these values belong to; the reverse of `new`.
    pub fn data_type(&self) -> DataType {
        match self {
            VectorData::Int64(_) => DataType::Int64,
            VectorData::Float64(_) => DataType::Float64,
            VectorData::Date(_) => DataType::Date,
            VectorData::Varchar(_) => DataType::Varchar,
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
// One column's values plus validity. In execution it holds up to VECTOR_SIZE (2048) rows;
// in storage, one Vector of num_rows rows is what a PAX column chunk encodes (see storage/pax.rs).
#[derive(Debug, PartialEq)]
pub struct Vector {
    pub data: VectorData, // values of one type
    pub validity: Bitmap, // one bit per value: which of those values are NULL
}
// A batch of rows across several columns, the unit operators pass to each other.
// A PAX row group (65,536 rows) splits into 32 DataChunks.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn null_count_partial_word() {
        let mut bitmap = Bitmap::all_valid(100); // 2 words, 28 padding bits stay 1
        assert_eq!(bitmap.null_count(100), 0);
        for i in [0, 63, 64, 99] {
            bitmap.set_null(i);
        }
        assert_eq!(bitmap.null_count(100), 4);
    }

    #[test]
    fn null_count_full_words() {
        let mut bitmap = Bitmap::all_valid(128); // no partial word
        bitmap.set_null(127);
        assert_eq!(bitmap.null_count(128), 1);
    }

    #[test]
    fn null_count_zero_rows() {
        assert_eq!(Bitmap::all_valid(0).null_count(0), 0);
    }

    #[test]
    fn null_count_ignores_cleared_padding() {
        // Padding cleared to 0 must not be counted as NULLs.
        let bitmap = Bitmap {
            words: vec![u64::MAX, 0],
        };
        assert_eq!(bitmap.null_count(64), 0);
    }

    #[test]
    #[should_panic(expected = "null_count")]
    fn null_count_too_many_rows() {
        Bitmap::all_valid(64).null_count(65);
    }
}
