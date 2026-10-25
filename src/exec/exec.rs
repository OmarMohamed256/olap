use std::{cmp::Ordering, ops::Bound};

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
    pub fn slice(&self, start: usize, count: usize) -> Bitmap {
        assert!(
            start % 64 == 0,
            "slice: start {start} is not a multiple of 64"
        );
        let first_word = start / 64;
        let num_words = Bitmap::num_words(count);
        assert!(
            first_word + num_words <= self.words.len(),
            "slice: {count} rows from row {start} but bitmap has only {} words",
            self.words.len()
        );

        // ALLOC: 32 words = 256 B per column per DataChunk; small
        let mut words = self.words[first_word..first_word + num_words].to_vec();
        // Padding bits past `count` are set to 1, like all_valid, so equal bitmaps
        // compare equal word for word and encode to the same bytes.
        if count & 63 > 0 {
            let mask: u64 = (1u64 << (count & 63)) - 1; // 1s on the real rows
            words[num_words - 1] |= !mask;
        }
        Bitmap { words }
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
// One value of a column, e.g. a zone map min/max or a filter bound.
// Variants mirror catalog::DataType one-to-one.
// No PartialOrd derive: it would order by variant first, so I64(1000) < F64(0.5).
#[derive(Debug, PartialEq, Clone)]
pub enum Scalar {
    I64(i64),
    F64(f64),
    Date(i32), // days since 1970-01-01, same as catalog::DataType::Date
    String(String),
}

impl Scalar {
    // None when the two can't be ordered: different types, or a NaN.
    pub fn compare(&self, other: &Scalar) -> Option<Ordering> {
        match (self, other) {
            (Scalar::I64(a), Scalar::I64(b)) => Some(a.cmp(b)),
            // f64 has NaN, which comes from things like
            // 0.0 / 0.0 or (-1.0).sqrt().
            // By the IEEE 754 standard, which every CPU follows,
            // NaN is not less than, equal to or greater than anything,
            // not even itself
            // So for floats, "which is bigger?" sometimes has no answer.
            // That's a partial order, and Rust expresses it with the trait PartialOrd.
            // Its method partial_cmp returns Option<Ordering>: Some(...) when there's an answer,
            // and None when NaN is involved.
            (Scalar::F64(a), Scalar::F64(b)) => a.partial_cmp(b),
            (Scalar::Date(a), Scalar::Date(b)) => Some(a.cmp(b)),
            (Scalar::String(a), Scalar::String(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnRange {
    pub column: usize,
    pub low: Bound<Scalar>,
    pub high: Bound<Scalar>,
}

// The values of one column, without validity (that lives in Vector.validity).
// NULL rows still take a slot so value i always belongs to row i.
#[derive(Debug, PartialEq, Clone)]
pub enum VectorData {
    Int64(Vec<i64>), // 8 bytes per value, packed
    Float64(Vec<f64>),
    Date(Vec<i32>), // days since 1970-01-01
    Varchar(StringVector),
}
// mimic on disk strings
#[derive(Debug, PartialEq, Clone)]
pub struct StringVector {
    offsets: Vec<u32>, // n + 1 entries; value i = data[offsets[i]..offsets[i + 1]]
    data: String,      // all values back to back
}
impl StringVector {
    pub fn new() -> Self {
        StringVector {
            offsets: vec![0],
            data: String::new(),
        }
    }
    pub fn push(&mut self, value: &str) {
        self.data.push_str(value);
        let offset = u32::try_from(self.data.len()).expect("offset does not fit in u32");
        self.offsets.push(offset);
    }
    // for reading only
    pub fn offsets(&self) -> &[u32] {
        &self.offsets
    }
    // for reading only
    pub fn data(&self) -> &str {
        &self.data
    }
    pub fn len(&self) -> usize {
        self.offsets.len() - 1
    }
    pub fn get(&self, i: usize) -> &str {
        let start = self.offsets[i] as usize;
        let end = self.offsets[i + 1] as usize;
        &self.data[start..end]
    }
    pub fn slice(&self, start: usize, count: usize) -> StringVector {
        assert!(
            start + count <= self.len(),
            "slice: start {start} and count {count} are out of index"
        );
        let first = self.offsets[start];
        let last = self.offsets[start + count];
        let data = (&self.data[first as usize..last as usize]).to_string();
        let offsets = self.offsets[start..=start + count]
            .iter()
            .map(|&o| o - first)
            .collect();

        StringVector { offsets, data }
    }

    // Builds from offsets and data read from a file, checking the layout rules once.
    // Takes ownership, so neither buffer is copied. data is already valid UTF-8 as a whole;
    // the boundary check catches an offset that cuts a character in half.
    pub fn from_parts(offsets: Vec<u32>, data: String) -> Result<StringVector, String> {
        let (Some(&first), Some(&last)) = (offsets.first(), offsets.last()) else {
            return Err("Corrupted data: string offsets are empty".to_string());
        };
        if first != 0 {
            return Err(format!(
                "Corrupted data: first string offset must be 0, got {first}"
            ));
        }
        for (i, pair) in offsets.windows(2).enumerate() {
            if pair[1] < pair[0] {
                return Err(format!(
                    "Corrupted data: string offset {} is smaller than the one before it",
                    i + 1
                ));
            }
        }
        if last as usize != data.len() {
            return Err(format!(
                "Corrupted data: last string offset is {last} but data has {} bytes",
                data.len()
            ));
        }
        for (i, &o) in offsets.iter().enumerate() {
            if !data.is_char_boundary(o as usize) {
                return Err(format!(
                    "Corrupted data: string offset {i} is not on a character boundary"
                ));
            }
        }
        Ok(StringVector { offsets, data })
    }

    #[cfg(test)]
    pub fn from_strs(values: &[&str]) -> Self {
        let mut v = StringVector::new();
        for s in values {
            v.push(s);
        }
        v
    }
}

// Lets tests build one with collect(), e.g. (0..100).map(|i| format!("s{i}")).collect().
#[cfg(test)]
impl FromIterator<String> for StringVector {
    fn from_iter<I: IntoIterator<Item = String>>(iter: I) -> Self {
        let mut v = StringVector::new();
        for s in iter {
            v.push(&s);
        }
        v
    }
}

impl VectorData {
    pub fn new(ty: DataType) -> Self {
        match ty {
            DataType::Int64 => VectorData::Int64(Vec::new()),
            DataType::Float64 => VectorData::Float64(Vec::new()),
            DataType::Date => VectorData::Date(Vec::new()),
            DataType::Varchar => VectorData::Varchar(StringVector::new()),
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
    /// Copies rows start..start + count into a new VectorData; the source is left unchanged.
    /// Still &mut self from the per-String days; can become &self once Fix 5 is measured.
    pub fn take_range(&self, start: usize, count: usize) -> Self {
        let range = start..start + count;
        match self {
            // ALLOC: fixed width is copied, one memcpy per column per DataChunk; cheap, and the
            // rows left behind are simply never read.
            VectorData::Int64(v) => VectorData::Int64(v[range].to_vec()),
            VectorData::Float64(v) => VectorData::Float64(v[range].to_vec()),
            VectorData::Date(v) => VectorData::Date(v[range].to_vec()),
            // ALLOC: two buffers (offsets, bytes) per DataChunk, one memcpy each.
            VectorData::Varchar(v) => VectorData::Varchar(v.slice(start, count)),
        }
    }
    pub fn format(&self, row: usize) -> String {
        match self {
            VectorData::Int64(v) => v[row].to_string(),
            VectorData::Float64(v) => v[row].to_string(),
            VectorData::Date(v) => format_date(v[row]),
            VectorData::Varchar(v) => v.get(row).to_string(),
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
impl Vector {
    // Copies rows start..start + count into a new Vector (see VectorData::take_range),
    // values and bitmap. start must be a multiple of 64 (Scan takes at multiples of 2048).
    pub fn take_range(&mut self, start: usize, count: usize) -> Vector {
        // Bitmap first: its asserts are cheap and give clear messages before any values are moved.
        let validity = self.validity.slice(start, count);
        Vector {
            data: self.data.take_range(start, count),
            validity,
        }
    }
}

// A batch of rows across several columns, the unit operators pass to each other.
// A PAX row group (65,536 rows) splits into 32 DataChunks.
pub struct DataChunk {
    pub columns: Vec<Vector>, // one Vector per column, e.g. 16 for SELECT * FROM lineitem
    pub count: usize,         // how many rows are filled in (≤ 2048)
}

pub trait Operator {
    fn next(&mut self) -> Result<Option<DataChunk>, String>;
}

pub trait TableSource {
    fn num_row_groups(&self) -> usize;
    fn row_group_num_rows(&self, rg: usize) -> usize;
    fn read_row_group(&self, rg: usize, columns: &[usize]) -> Result<Vec<Vector>, String>;
    // Could row group rg hold a row inside every range? true = must read it. The default
    // (no statistics, e.g. CSV) can never rule a group out.
    fn may_match(&self, _rg: usize, _pushed_filters: &[ColumnRange]) -> bool {
        true
    }
}

pub struct Scan {
    source: Box<dyn TableSource>,
    columns: Vec<usize>, // which columns to read, e.g. vec![4, 6] for l_quantity, l_discount
    // Used only to skip row groups via the source's statistics; rows that fail can still be
    // returned, so a Filter above must check them.
    pushed_filters: Vec<ColumnRange>,
    current: Vec<Vector>, // decoded columns of the loaded row group, one per entry of columns; empty before the first load
    curret_rg: usize,     // next rg to load
    position: usize,      // row in the loaded group to hand out next
    current_rows: usize,  // how many rows the loaded group has
    skipped_groups: usize, // groups pushed_filters ruled out, never read
}

impl Scan {
    pub fn new(
        source: Box<dyn TableSource>,
        columns: Vec<usize>,
        pushed_filters: Vec<ColumnRange>,
    ) -> Self {
        Scan {
            source,
            columns,
            pushed_filters,
            current: Vec::new(),
            curret_rg: 0,
            position: 0,
            current_rows: 0,
            skipped_groups: 0,
        }
    }

    pub fn skipped_groups(&self) -> usize {
        self.skipped_groups
    }
}

// Scan(Table) : Dataflow
// The Scan operator retrieves data vector-at-a-time from the table source
impl Operator for Scan {
    fn next(&mut self) -> Result<Option<DataChunk>, String> {
        // Load the next row group until one has rows left (or none remain).
        while self.position == self.current_rows {
            if self.curret_rg == self.source.num_row_groups() {
                return Ok(None); // every group has been read
            }
            // A skipped group changes nothing but curret_rg: position == current_rows still
            // holds, so the loop moves on instead of handing out the old group's rows again.
            let may_match = self.source.may_match(self.curret_rg, &self.pushed_filters);
            if may_match {
                // FREE: assigning drops the previous group: its large Vecs (munmap), Varchar
                // included, since each column is just two buffers now.
                self.current = self.source.read_row_group(self.curret_rg, &self.columns)?;
                self.current_rows = self.source.row_group_num_rows(self.curret_rg); // same group as the read
                // Vectors that don't match the source's own row count are a source bug: say so here,
                // not later as a confusing take_range panic.
                debug_assert!(
                    self.current
                        .iter()
                        .all(|v| v.data.len() == self.current_rows),
                    "row group {} returned Vectors that don't have {} rows",
                    self.curret_rg,
                    self.current_rows
                );
                self.position = 0; // a new group starts at its first row
            } else {
                self.skipped_groups += 1;
            }
            self.curret_rg += 1;
        }
        // Hand out the next piece: up to VECTOR_SIZE rows from position.
        let count = std::cmp::min(VECTOR_SIZE, self.current_rows - self.position);
        // cuts each col into 2048 pieces and passes them up the operator tree
        // ALLOC: one Vec<Vector> per DataChunk plus, per column, a bitmap copy and a Vec
        // (fixed width: a copy; Varchar: offsets and one byte range, see StringVector::slice).
        let columns = self
            .current
            .iter_mut()
            .map(|col| col.take_range(self.position, count))
            .collect();

        self.position += count;
        Ok(Some(DataChunk { columns, count }))
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

    // 200 rows with NULLs on both sides of word boundaries and past row 100.
    fn bitmap_200() -> Bitmap {
        let mut bitmap = Bitmap::all_valid(200);
        for i in [5, 63, 64, 100, 127, 128, 199] {
            bitmap.set_null(i);
        }
        bitmap
    }

    #[test]
    fn slice_matches_source_rows() {
        let source = bitmap_200();
        let slice = source.slice(64, 136); // rows 64..200
        for i in 0..136 {
            assert_eq!(slice.is_valid(i), source.is_valid(64 + i), "row {i}");
        }
        assert_eq!(slice.null_count(136), 5); // 64, 100, 127, 128, 199
    }

    #[test]
    fn slice_padding_is_one() {
        // Rows 0..100 end mid-word; the source has a NULL at row 100, right past the end.
        let slice = bitmap_200().slice(0, 100);
        let mut expected = Bitmap::all_valid(100);
        for i in [5, 63, 64] {
            expected.set_null(i);
        }
        assert_eq!(slice, expected);
    }

    #[test]
    fn slice_full_words() {
        let slice = bitmap_200().slice(128, 64); // exactly word 2, no padding to fix
        assert_eq!(slice.words, vec![bitmap_200().words[2]]);
    }

    #[test]
    fn slice_zero_rows() {
        assert!(bitmap_200().slice(64, 0).words.is_empty());
    }

    #[test]
    #[should_panic(expected = "multiple of 64")]
    fn slice_rejects_unaligned_start() {
        bitmap_200().slice(10, 20);
    }

    // A Vector of `data` with NULLs at `nulls` (NULL slots keep whatever value they had).
    fn vector(data: VectorData, nulls: &[usize]) -> Vector {
        let mut validity = Bitmap::all_valid(data.len());
        for &i in nulls {
            validity.set_null(i);
        }
        Vector { data, validity }
    }

    #[test]
    fn take_range_int() {
        let mut source = vector(VectorData::Int64((0..200).collect()), &[5, 70, 100, 150]);
        // Rows 64..100: NULL at 70 lands at 6; the NULL at 100 is just past the end.
        let expected = vector(VectorData::Int64((64..100).collect()), &[6]);
        assert_eq!(source.take_range(64, 36), expected);
    }

    #[test]
    fn take_range_int_copies() {
        // Fixed width is copied, so the source keeps its values.
        let mut source = vector(VectorData::Int64((0..200).collect()), &[5]);
        source.take_range(64, 36);
        assert_eq!(source, vector(VectorData::Int64((0..200).collect()), &[5]));
    }

    #[test]
    fn take_range_varchar() {
        let strings: StringVector = (0..130).map(|i| format!("s{i}")).collect();
        let mut source = vector(VectorData::Varchar(strings), &[64, 66, 129]);
        let expected_strings = (64..130).map(|i| format!("s{i}")).collect();
        // Values and NULLs stay together: NULLs at 64, 66, 129 land at 0, 2, 65.
        let expected = vector(VectorData::Varchar(expected_strings), &[0, 2, 65]);
        assert_eq!(source.take_range(64, 66), expected);
    }

    #[test]
    fn take_range_varchar_copies() {
        // Varchar is copied now too (one byte range), so the source keeps its values.
        let strings: StringVector = (0..200).map(|i| format!("s{i}")).collect();
        let mut source = vector(VectorData::Varchar(strings.clone()), &[70]);
        source.take_range(64, 36);
        assert_eq!(source, vector(VectorData::Varchar(strings), &[70]));
    }

    #[test]
    fn take_range_last_short_range() {
        // A 4,464-row row group cut into 2048-row DataChunks: 2048, 2048, then 368.
        let mut source = vector(VectorData::Int64((0..4_464).collect()), &[4_096, 4_463]);
        let last = source.take_range(4_096, 368);
        assert_eq!(
            last,
            vector(VectorData::Int64((4_096..4_464).collect()), &[0, 367])
        );
        assert_eq!(last.validity.null_count(368), 2);
    }

    #[test]
    #[should_panic(expected = "multiple of 64")]
    fn take_range_rejects_unaligned_start() {
        let strings = (0..100).map(|i| format!("s{i}")).collect();
        vector(VectorData::Varchar(strings), &[]).take_range(10, 20);
    }

    #[test]
    #[should_panic(expected = "slice")]
    fn slice_too_many_rows() {
        bitmap_200().slice(128, 129); // 3 words from word 2, but only 4 words exist
    }

    #[test]
    fn compare_same_type() {
        use Ordering::*;
        let cases = [
            (Scalar::I64(-5), Scalar::I64(3), Less),
            (Scalar::I64(3), Scalar::I64(3), Equal),
            (Scalar::F64(2.5), Scalar::F64(-1.0), Greater),
            (Scalar::F64(0.0), Scalar::F64(-0.0), Equal), // IEEE: 0.0 == -0.0
            (Scalar::Date(8_766), Scalar::Date(9_131), Less), // 1994-01-01 < 1995-01-01
            (
                Scalar::String("AIR".to_string()),
                Scalar::String("TRUCK".to_string()),
                Less,
            ),
            (
                Scalar::String("".to_string()),
                Scalar::String("AIR".to_string()),
                Less,
            ),
            // Byte order: uppercase (0x41..) sorts before lowercase (0x61..)
            (
                Scalar::String("a".to_string()),
                Scalar::String("Z".to_string()),
                Greater,
            ),
        ];
        for (a, b, expected) in cases {
            assert_eq!(a.compare(&b), Some(expected), "{a:?} vs {b:?}");
            assert_eq!(b.compare(&a), Some(expected.reverse()), "{b:?} vs {a:?}");
        }
    }

    #[test]
    fn compare_nan_is_none() {
        let nan = Scalar::F64(f64::NAN);
        assert_eq!(nan.compare(&Scalar::F64(1.0)), None);
        assert_eq!(Scalar::F64(1.0).compare(&nan), None);
        assert_eq!(nan.compare(&nan), None); // not even equal to itself
    }

    #[test]
    fn compare_mixed_types_is_none() {
        // A derived PartialOrd would order these by variant and call I64 smaller.
        assert_eq!(Scalar::I64(1000).compare(&Scalar::F64(0.5)), None);
        assert_eq!(Scalar::I64(1).compare(&Scalar::F64(1.0)), None);
        assert_eq!(Scalar::Date(1).compare(&Scalar::I64(1)), None);
        assert_eq!(
            Scalar::String("1".to_string()).compare(&Scalar::I64(1)),
            None
        );
    }

    #[test]
    fn string_vector_empty() {
        assert_eq!(StringVector::new().len(), 0);
    }

    #[test]
    fn string_vector_push_get() {
        let mut v = StringVector::new();
        for s in ["AIR", "", "TRUCK", "café"] {
            v.push(s);
        }
        assert_eq!(v.len(), 4); // values, not bytes (would be 13)
        assert_eq!(v.get(0), "AIR");
        assert_eq!(v.get(1), "");
        assert_eq!(v.get(2), "TRUCK");
        assert_eq!(v.get(3), "café"); // 5 bytes, 4 chars
    }

    #[test]
    fn string_vector_only_empty_values() {
        // Empty values add offsets but no bytes.
        let mut v = StringVector::new();
        v.push("");
        v.push("");
        assert_eq!(v.len(), 2);
        assert_eq!(v.get(0), "");
        assert_eq!(v.get(1), "");
    }

    #[test]
    fn string_vector_from_strs() {
        let mut pushed = StringVector::new();
        pushed.push("a");
        pushed.push("");
        pushed.push("bc");
        assert_eq!(StringVector::from_strs(&["a", "", "bc"]), pushed);
    }

    fn air_truck_ship() -> StringVector {
        StringVector::from_strs(&["AIR", "", "TRUCK", "SHIP"]) // offsets [0, 3, 3, 8, 12]
    }

    #[test]
    fn string_vector_slice_middle() {
        let slice = air_truck_ship().slice(2, 2);
        assert_eq!(slice.offsets, [0, 5, 9]); // rebased: [3, 8, 12] minus 3
        assert_eq!(slice, StringVector::from_strs(&["TRUCK", "SHIP"]));
    }

    #[test]
    fn string_vector_slice_from_start() {
        let slice = air_truck_ship().slice(0, 2);
        assert_eq!(slice, StringVector::from_strs(&["AIR", ""]));
    }

    #[test]
    fn string_vector_slice_at_end() {
        let slice = air_truck_ship().slice(3, 1);
        assert_eq!(slice, StringVector::from_strs(&["SHIP"]));
    }

    #[test]
    fn string_vector_slice_empty() {
        let slice = air_truck_ship().slice(1, 0);
        assert_eq!(slice.len(), 0);
        assert_eq!(slice, StringVector::new()); // offsets [0]
    }

    #[test]
    fn string_vector_slice_whole() {
        let source = air_truck_ship();
        assert_eq!(source.slice(0, source.len()), source);
    }

    #[test]
    fn string_vector_slice_leaves_source() {
        let source = air_truck_ship();
        source.slice(1, 2);
        assert_eq!(source, air_truck_ship());
    }

    #[test]
    fn string_vector_slice_multibyte() {
        let slice = StringVector::from_strs(&["a", "café", "b"]).slice(1, 1);
        assert_eq!(slice.get(0), "café");
        assert_eq!(slice.offsets, [0, 5]); // bytes, not chars
    }

    #[test]
    #[should_panic(expected = "slice")]
    fn string_vector_slice_out_of_range() {
        air_truck_ship().slice(3, 2);
    }

    fn from_parts_err(offsets: Vec<u32>, data: &str) -> String {
        StringVector::from_parts(offsets, data.to_string()).unwrap_err()
    }

    #[test]
    fn string_vector_from_parts() {
        let v = StringVector::from_parts(vec![0, 3, 3, 8], "AIRTRUCK".to_string()).unwrap();
        assert_eq!(v, StringVector::from_strs(&["AIR", "", "TRUCK"]));
    }

    #[test]
    fn string_vector_from_parts_empty() {
        let v = StringVector::from_parts(vec![0], String::new()).unwrap();
        assert_eq!(v, StringVector::new());
    }

    #[test]
    fn string_vector_from_parts_bad() {
        assert!(from_parts_err(vec![], "").contains("empty"));
        assert!(from_parts_err(vec![1, 3], "AIR").contains("must be 0"));
        assert!(from_parts_err(vec![0, 3, 2, 3], "AIR").contains("offset 2 is smaller"));
        assert!(from_parts_err(vec![0, 2], "AIR").contains("data has 3 bytes"));
        // "café": é is bytes 3..5, so offset 4 cuts it in half
        assert!(from_parts_err(vec![0, 4, 5], "café").contains("offset 1 is not on a character"));
    }

    #[test]
    #[should_panic]
    fn string_vector_get_out_of_range() {
        let mut v = StringVector::new();
        v.push("AIR");
        v.get(1);
    }
}
