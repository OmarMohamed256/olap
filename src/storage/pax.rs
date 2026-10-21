const MAGIC: [u8; 4] = *b"PAX1";
// Rows per row group: 32 DataChunks of 2048. A multiple of 64, so each group's bitmap starts on a word boundary.
const ROW_GROUP_SIZE: usize = 65_536;
use crate::{
    catalog::{Column, DataType, TableSchema},
    exec::exec::{Bitmap, Vector, VectorData},
};
use std::fs::{self, File};
use std::io::Write; // brings write_all into scope

impl Column {
    // A helper to make creating columns less verbose
    pub fn new(name: &str, ty: DataType) -> Self {
        Self {
            name: name.to_string(),
            ty,
        }
    }
}
impl TableSchema {
    pub fn new(name: &str, columns: Vec<Column>) -> Self {
        Self {
            name: name.to_string(),
            columns,
        }
    }

    // takes a buffer and encodes the table scheme into bytes and appends them to the buffer
    pub fn encode(&self, buffer: &mut Vec<u8>) {
        // storing the name of table can be ommited as table already is the filename but we keep for practice
        buffer.extend_from_slice(&(self.name.len() as u32).to_le_bytes());
        buffer.extend_from_slice(self.name.as_bytes());

        buffer.extend_from_slice(&(self.columns.len() as u16).to_le_bytes());
        self.columns.iter().for_each(|col| {
            buffer.extend_from_slice(&(col.name.len() as u32).to_le_bytes());
            buffer.extend_from_slice(col.name.as_bytes());
            buffer.push(col.ty as u8);
        });
    }
    // take the buffer and consume bytes as you go
    pub fn decode(buffer: &mut &[u8]) -> Result<Self, String> {
        // 1. Table name length
        let len_bytes = take_bytes(buffer, 4, "Unexpected end reading table name length")?;
        let table_name_length = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;

        // 2. Table name string
        let name_bytes = take_bytes(
            buffer,
            table_name_length,
            "Unexpected end reading table name",
        )?;
        let table_name = std::str::from_utf8(name_bytes)
            .map_err(|_| "Invalid UTF-8 in table name".to_string())?
            .to_string();

        // 3. Column count
        let columns_size = take_bytes(buffer, 2, "Unexpected end reading columns count")?;
        let parsed_column_count = u16::from_le_bytes(columns_size.try_into().unwrap()) as usize;

        let mut columns: Vec<Column> = Vec::with_capacity(parsed_column_count);

        // 4. Parse columns
        for _ in 0..parsed_column_count {
            // Column name length
            let col_name_len_bytes =
                take_bytes(buffer, 4, "Unexpected end reading column name length")?;
            let col_name_len = u32::from_le_bytes(col_name_len_bytes.try_into().unwrap()) as usize;

            // Column name string
            let column_name_bytes =
                take_bytes(buffer, col_name_len, "Unexpected end reading column name")?;
            let column_name = std::str::from_utf8(column_name_bytes)
                .map_err(|_| "Invalid UTF-8 in column name".to_string())?
                .to_string();

            // Column type
            let column_type_bytes = take_bytes(buffer, 1, "Unexpected end reading column type")?;
            let type_byte = column_type_bytes[0];

            let col_type = match type_byte {
                1 => DataType::Int64,
                2 => DataType::Float64,
                3 => DataType::Date,
                4 => DataType::Varchar,
                _ => {
                    return Err(format!(
                        "Corrupted data: unknown DataType byte {}",
                        type_byte
                    ));
                }
            };

            columns.push(Column {
                name: column_name,
                ty: col_type,
            });
        }

        Ok(Self {
            name: table_name,
            columns,
        })
    }
}

// One value of a column, used for zone map min/max.
// Variants mirror catalog::DataType one-to-one.
#[derive(Debug, PartialEq)]
pub enum Scalar {
    I64(i64),
    F64(f64),
    Date(i32), // days since 1970-01-01, same as catalog::DataType::Date
    String(String),
}
impl Scalar {
    // No type tag is written: the decoder learns the type from the schema column.
    pub fn encode(&self, buffer: &mut Vec<u8>) {
        match self {
            Scalar::I64(v) => buffer.extend_from_slice(&v.to_le_bytes()),
            Scalar::F64(v) => buffer.extend_from_slice(&v.to_le_bytes()),
            Scalar::Date(v) => buffer.extend_from_slice(&v.to_le_bytes()),
            Scalar::String(v) => {
                buffer.extend_from_slice(&(v.len() as u32).to_le_bytes());
                buffer.extend_from_slice(v.as_bytes());
            }
        }
    }

    // `ty` comes from the schema column, since no type tag is stored.
    pub fn decode(buffer: &mut &[u8], ty: DataType) -> Result<Self, String> {
        match ty {
            DataType::Int64 => {
                let bytes = take_bytes(buffer, 8, "Unexpected end reading i64 value")?;
                Ok(Scalar::I64(i64::from_le_bytes(bytes.try_into().unwrap())))
            }
            DataType::Float64 => {
                let bytes = take_bytes(buffer, 8, "Unexpected end reading f64 value")?;
                Ok(Scalar::F64(f64::from_le_bytes(bytes.try_into().unwrap())))
            }
            DataType::Date => {
                let bytes = take_bytes(buffer, 4, "Unexpected end reading date value")?;
                Ok(Scalar::Date(i32::from_le_bytes(bytes.try_into().unwrap())))
            }
            DataType::Varchar => {
                // 1. String length
                let len_bytes = take_bytes(buffer, 4, "Unexpected end reading string length")?;
                let len = u32::from_le_bytes(len_bytes.try_into().unwrap()) as usize;

                // 2. String bytes
                let str_bytes = take_bytes(buffer, len, "Unexpected end reading string value")?;
                let value = std::str::from_utf8(str_bytes)
                    .map_err(|_| "Invalid UTF-8 in string value".to_string())?
                    .to_string();
                Ok(Scalar::String(value))
            }
        }
    }

    // Since there is no tag on disk, a Scalar of the wrong type would decode as garbage.
    fn matches(&self, ty: DataType) -> bool {
        matches!(
            (self, ty),
            (Scalar::I64(_), DataType::Int64)
                | (Scalar::F64(_), DataType::Float64)
                | (Scalar::Date(_), DataType::Date)
                | (Scalar::String(_), DataType::Varchar)
        )
    }
}

// Always holds both values; "no zone map" is ChunkMetadata.zone_map == None.
#[derive(Debug, PartialEq)]
pub struct ZoneMap {
    pub min: Scalar,
    pub max: Scalar,
}
impl ZoneMap {
    pub fn encode(&self, buffer: &mut Vec<u8>) {
        self.min.encode(buffer);
        self.max.encode(buffer);
    }

    pub fn decode(buffer: &mut &[u8], ty: DataType) -> Result<Self, String> {
        let min = Scalar::decode(buffer, ty)?;
        let max = Scalar::decode(buffer, ty)?;
        Ok(Self { min, max })
    }
}

#[derive(Debug, PartialEq)]
pub struct ChunkMetadata {
    pub chunk_length: u32,
    pub offset: u64,
    pub null_count: u32,
    pub zone_map: Option<ZoneMap>, // None when every value in the chunk is NULL
}
impl ChunkMetadata {
    pub fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.chunk_length.to_le_bytes());
        buffer.extend_from_slice(&self.offset.to_le_bytes());
        buffer.extend_from_slice(&self.null_count.to_le_bytes());
        // On disk an Option is a presence byte (0/1), then the value only if present.
        match &self.zone_map {
            Some(zone_map) => {
                buffer.push(1);
                zone_map.encode(buffer);
            }
            None => buffer.push(0),
        }
    }

    // `ty` is the type of the schema column this chunk belongs to.
    pub fn decode(buffer: &mut &[u8], ty: DataType) -> Result<Self, String> {
        // 1. Chunk length
        let len_bytes = take_bytes(buffer, 4, "Unexpected end reading chunk length")?;
        let chunk_length = u32::from_le_bytes(len_bytes.try_into().unwrap());

        // 2. Offset
        let offset_bytes = take_bytes(buffer, 8, "Unexpected end reading chunk offset")?;
        let offset = u64::from_le_bytes(offset_bytes.try_into().unwrap());

        // 3. Null count
        let null_bytes = take_bytes(buffer, 4, "Unexpected end reading null count")?;
        let null_count = u32::from_le_bytes(null_bytes.try_into().unwrap());

        // 4. Zone map: presence byte, then min/max only if present
        let present_bytes = take_bytes(buffer, 1, "Unexpected end reading zone map flag")?;
        let present_byte = present_bytes[0];

        let zone_map = match present_byte {
            0 => None,
            1 => Some(ZoneMap::decode(buffer, ty)?),
            _ => {
                return Err(format!(
                    "Corrupted data: zone map flag must be 0 or 1, got {}",
                    present_byte
                ));
            }
        };

        Ok(Self {
            chunk_length,
            offset,
            null_count,
            zone_map,
        })
    }
}

// No row_group_id: a row group's id is its index in PaxFooter.row_groups_metadata,
// the same position rule that links chunks to schema columns. Storing it would
// only duplicate what the order already says.
#[derive(Debug, PartialEq)]
pub struct RowGroupMeta {
    pub num_rows: u32,
    pub total_byte_size: u64,
    pub chunks_metadata: Vec<ChunkMetadata>, // one per schema column, in schema order
}
impl RowGroupMeta {
    pub fn encode(&self, buffer: &mut Vec<u8>) {
        buffer.extend_from_slice(&self.num_rows.to_le_bytes());
        buffer.extend_from_slice(&self.total_byte_size.to_le_bytes());

        // no need for chunks length as table scheme already records that and we can match by indexing
        self.chunks_metadata
            .iter()
            .for_each(|chunk| chunk.encode(buffer));
    }

    // `columns` is the schema: it gives the chunk count and each chunk's type.
    pub fn decode(buffer: &mut &[u8], columns: &[Column]) -> Result<Self, String> {
        // 1. Number of rows
        let rows_bytes = take_bytes(buffer, 4, "Unexpected end reading row count")?;
        let num_rows = u32::from_le_bytes(rows_bytes.try_into().unwrap());

        // 2. Total byte size
        let size_bytes = take_bytes(buffer, 8, "Unexpected end reading row group size")?;
        let total_byte_size = u64::from_le_bytes(size_bytes.try_into().unwrap());

        // 3. One chunk per schema column, in schema order
        let mut chunks_metadata: Vec<ChunkMetadata> = Vec::with_capacity(columns.len());
        for i in 0..columns.len() {
            let chunk = ChunkMetadata::decode(buffer, columns[i].ty)?;
            chunks_metadata.push(chunk);
        }

        Ok(Self {
            num_rows,
            total_byte_size,
            chunks_metadata,
        })
    }
}

// will be loaded on first access
#[derive(Debug, PartialEq)]
pub struct PaxFooter {
    row_groups_metadata: Vec<RowGroupMeta>,
    scheme: TableSchema,
}
impl PaxFooter {
    pub fn new(scheme: TableSchema, row_groups_metadata: Vec<RowGroupMeta>) -> Self {
        Self {
            scheme,
            row_groups_metadata,
        }
    }

    // Schema goes first: decoding a row group needs it for the chunk count
    // and for each zone map's type.
    pub fn encode(&self, buffer: &mut Vec<u8>) -> Result<(), String> {
        // Check everything before writing, so a bad footer leaves the buffer untouched.
        let columns = &self.scheme.columns;
        for rg in 0..self.row_groups_metadata.len() {
            let row_group = &self.row_groups_metadata[rg];
            let chunk_count = row_group.chunks_metadata.len();

            if chunk_count != columns.len() {
                return Err(format!(
                    "row group {rg} has {chunk_count} chunks, schema has {} columns",
                    columns.len()
                ));
            }

            for i in 0..columns.len() {
                let chunk = &row_group.chunks_metadata[i];
                let column = &columns[i];

                match &chunk.zone_map {
                    None => {} // all-NULL chunk, nothing to check
                    Some(zone_map) => {
                        let min_ok = zone_map.min.matches(column.ty);
                        let max_ok = zone_map.max.matches(column.ty);
                        if !min_ok || !max_ok {
                            return Err(format!(
                                "row group {rg}: zone map type does not match column {}",
                                column.name
                            ));
                        }
                    }
                }
            }
        }

        self.scheme.encode(buffer);
        buffer.extend_from_slice(&(self.row_groups_metadata.len() as u32).to_le_bytes());
        self.row_groups_metadata
            .iter()
            .for_each(|row_group| row_group.encode(buffer));
        Ok(())
    }

    // `buffer` must hold exactly the footer bytes (the reader cuts them out using the footer length).
    pub fn decode(buffer: &mut &[u8]) -> Result<Self, String> {
        // 1. Schema first: row groups need it to decode
        let scheme = TableSchema::decode(buffer)?;

        // 2. Row group count
        let count_bytes = take_bytes(buffer, 4, "Unexpected end reading row group count")?;
        let row_group_count = u32::from_le_bytes(count_bytes.try_into().unwrap()) as usize;

        // 3. Row groups
        // No with_capacity here: a corrupted count (up to ~4 billion) would try to
        // reserve gigabytes up front. Vec::new() grows only as row groups really decode.
        let mut row_groups_metadata: Vec<RowGroupMeta> = Vec::new();
        for _ in 0..row_group_count {
            let row_group = RowGroupMeta::decode(buffer, &scheme.columns)?;
            row_groups_metadata.push(row_group);
        }

        // 4. Nothing may be left over, or the footer length and contents disagree
        if !buffer.is_empty() {
            return Err(format!(
                "Corrupted data: {} unexpected bytes after footer",
                buffer.len()
            ));
        }

        Ok(Self {
            scheme,
            row_groups_metadata,
        })
    }
}

// How execution types map to the file layout:
//   row group      (all columns, up to 65,536 rows) -> split into 32 DataChunks when scanned
//   column chunk   (one column of a row group)      -> one Vector: values + validity
//   values section of a column chunk                -> VectorData
//   validity section (only if null_count > 0)       -> Bitmap
impl VectorData {
    pub fn encode(&self, buffer: &mut Vec<u8>) {
        self.encode_range(0, self.len(), buffer);
    }

    // Encodes only rows start..start + num_rows, so the writer can write one row group
    // of a whole-table column without copying it first. `&v[range]` is a view, not a copy.
    pub fn encode_range(&self, start: usize, num_rows: usize, buffer: &mut Vec<u8>) {
        let range = start..start + num_rows;
        match self {
            VectorData::Int64(v) => {
                let v = &v[range];
                buffer.reserve(v.len() * 8);
                for &val in v {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            VectorData::Float64(v) => {
                let v = &v[range];
                buffer.reserve(v.len() * 8);
                for &val in v {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            VectorData::Date(v) => {
                let v = &v[range];
                buffer.reserve(v.len() * 4);
                for &val in v {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            VectorData::Varchar(v) => {
                let v = &v[range]; // borrows the Strings, no clone: offsets restart at 0 per chunk
                let total_bytes: usize = v.iter().map(|s| s.len()).sum();
                buffer.reserve(4 + (v.len() * 4) + total_bytes);

                let mut offset: u32 = 0;
                buffer.extend_from_slice(&offset.to_le_bytes());

                // Pass 2: Write all offsets sequentially
                for s in v {
                    offset += s.len() as u32;
                    buffer.extend_from_slice(&offset.to_le_bytes());
                }

                // Pass 3: Write all string payloads sequentially
                for s in v {
                    buffer.extend_from_slice(s.as_bytes());
                }
            }
        }
    }
    pub fn decode(buffer: &mut &[u8], ty: DataType, num_rows: usize) -> Result<Self, String> {
        match ty {
            DataType::Int64 => {
                let bytes = take_bytes(buffer, 8 * num_rows, "Unexpected end reading i64 values")?;
                let mut values = Vec::with_capacity(num_rows); // safe: take_bytes already proved the bytes exist
                for piece in bytes.chunks_exact(8) {
                    values.push(i64::from_le_bytes(piece.try_into().unwrap()));
                }
                Ok(VectorData::Int64(values))
            }
            DataType::Float64 => {
                let bytes = take_bytes(buffer, 8 * num_rows, "Unexpected end reading f64 values")?;
                let mut values = Vec::with_capacity(num_rows);
                for piece in bytes.chunks_exact(8) {
                    values.push(f64::from_le_bytes(piece.try_into().unwrap()));
                }
                Ok(VectorData::Float64(values))
            }
            DataType::Date => {
                let bytes = take_bytes(buffer, 4 * num_rows, "Unexpected end reading date values")?;
                let mut values = Vec::with_capacity(num_rows);
                for piece in bytes.chunks_exact(4) {
                    values.push(i32::from_le_bytes(piece.try_into().unwrap()));
                }
                Ok(VectorData::Date(values))
            }
            DataType::Varchar => {
                // 1. Offsets: num_rows + 1 of them; string i is data[offsets[i]..offsets[i + 1]]
                let offset_bytes = take_bytes(
                    buffer,
                    4 * (num_rows + 1),
                    "Unexpected end reading string offsets",
                )?;
                let mut offsets: Vec<usize> = Vec::with_capacity(num_rows + 1);
                for piece in offset_bytes.chunks_exact(4) {
                    offsets.push(u32::from_le_bytes(piece.try_into().unwrap()) as usize);
                }

                // 2. Check offsets before slicing with them, so bad data returns Err instead of panicking
                if offsets[0] != 0 {
                    return Err(format!(
                        "Corrupted data: first string offset must be 0, got {}",
                        offsets[0]
                    ));
                }
                for i in 0..num_rows {
                    if offsets[i + 1] < offsets[i] {
                        return Err(format!(
                            "Corrupted data: string offset {} is smaller than the one before it",
                            i + 1
                        ));
                    }
                }

                // 3. Data area: the last offset is its total length
                let data_len = offsets[num_rows];
                let data = take_bytes(buffer, data_len, "Unexpected end reading string data")?;

                // 4. Cut each string out of the data area
                let mut values = Vec::with_capacity(num_rows);
                for i in 0..num_rows {
                    let value_bytes = &data[offsets[i]..offsets[i + 1]];
                    let value = std::str::from_utf8(value_bytes)
                        .map_err(|_| "Invalid UTF-8 in string value".to_string())?
                        .to_string();
                    values.push(value);
                }

                Ok(VectorData::Varchar(values))
            }
        }
    }
}
impl Bitmap {
    pub fn encode(&self, buffer: &mut Vec<u8>) {
        self.encode_range(0, self.words.len() * 64, buffer);
    }

    // Writes the words covering rows start..start + num_rows. `start` must be a multiple
    // of 64 so the range is whole words; no bit shifting needed.
    pub fn encode_range(&self, start: usize, num_rows: usize, buffer: &mut Vec<u8>) {
        assert!(
            start % 64 == 0,
            "Bitmap::encode_range: start {start} is not a multiple of 64"
        );
        let first_word = start / 64;
        let words = &self.words[first_word..first_word + Bitmap::num_words(num_rows)];
        buffer.reserve(words.len() * 8);
        for &word in words {
            buffer.extend_from_slice(&word.to_le_bytes());
        }
    }

    pub fn decode(buffer: &mut &[u8], num_rows: usize) -> Result<Self, String> {
        let num_words = Bitmap::num_words(num_rows);
        let bytes = take_bytes(buffer, 8 * num_words, "Unexpected end reading bitmap")?;

        let mut words = Vec::with_capacity(num_words); // safe: take_bytes already proved the bytes exist
        for piece in bytes.chunks_exact(8) {
            words.push(u64::from_le_bytes(piece.try_into().unwrap()));
        }
        Ok(Bitmap { words })
    }
}
// One column chunk on disk: [bitmap, only if null_count > 0][values].
impl Vector {
    // Writes the chunk and returns its null_count for ChunkMetadata.
    // The count comes from the bitmap, so the footer can never disagree with the bytes.
    pub fn encode(&self, buffer: &mut Vec<u8>) -> u32 {
        self.encode_range(0, self.data.len(), buffer)
    }

    // Writes rows start..start + num_rows as one column chunk; the writer calls this once
    // per row group and column. `start` must be a multiple of 64 (row-group starts are).
    pub fn encode_range(&self, start: usize, num_rows: usize, buffer: &mut Vec<u8>) -> u32 {
        debug_assert_eq!(
            self.validity.words.len(),
            Bitmap::num_words(self.data.len())
        );
        let null_count = self.validity.null_count_range(start, num_rows);
        if null_count > 0 {
            self.validity.encode_range(start, num_rows, buffer);
        }
        self.data.encode_range(start, num_rows, buffer);
        null_count as u32
    }

    // Min/max of the non-NULL rows in start..start + num_rows, for one column chunk's zone map.
    // The caller must check null_count < num_rows first: an all-NULL chunk gets no zone map.
    pub fn zone_map_range(&self, start: usize, num_rows: usize) -> ZoneMap {
        let end = start + num_rows;
        // The first non-NULL row seeds min and max, so no sentinels like i64::MAX are needed.
        let mut first = start;
        while first < end && !self.validity.is_valid(first) {
            first += 1;
        }
        assert!(
            first < end,
            "zone_map_range: rows {start}..{end} are all NULL"
        );

        match &self.data {
            VectorData::Int64(v) => {
                let mut min = v[first];
                let mut max = v[first];
                for i in first + 1..end {
                    if self.validity.is_valid(i) {
                        min = min.min(v[i]);
                        max = max.max(v[i]);
                    }
                }
                ZoneMap {
                    min: Scalar::I64(min),
                    max: Scalar::I64(max),
                }
            }
            VectorData::Float64(v) => {
                // f64::min/max ignore NaN: min(NaN, 2.0) = 2.0. A chunk of only NaN gives NaN, NaN.
                let mut min = v[first];
                let mut max = v[first];
                for i in first + 1..end {
                    if self.validity.is_valid(i) {
                        min = min.min(v[i]);
                        max = max.max(v[i]);
                    }
                }
                ZoneMap {
                    min: Scalar::F64(min),
                    max: Scalar::F64(max),
                }
            }
            VectorData::Date(v) => {
                let mut min = v[first];
                let mut max = v[first];
                for i in first + 1..end {
                    if self.validity.is_valid(i) {
                        min = min.min(v[i]);
                        max = max.max(v[i]);
                    }
                }
                ZoneMap {
                    min: Scalar::Date(min),
                    max: Scalar::Date(max),
                }
            }
            VectorData::Varchar(v) => {
                // Borrow while scanning and clone only the two winners, not every string.
                // String comparison is byte-wise, which for UTF-8 equals code-point order.
                let mut min = &v[first];
                let mut max = &v[first];
                for i in first + 1..end {
                    if self.validity.is_valid(i) {
                        if v[i] < *min {
                            min = &v[i];
                        }
                        if v[i] > *max {
                            max = &v[i];
                        }
                    }
                }
                ZoneMap {
                    min: Scalar::String(min.clone()),
                    max: Scalar::String(max.clone()),
                }
            }
        }
    }

    pub fn decode(
        buffer: &mut &[u8],
        ty: DataType,
        num_rows: usize,
        null_count: u32,
    ) -> Result<Self, String> {
        let validity = if null_count > 0 {
            let bitmap = Bitmap::decode(buffer, num_rows)?;
            // The footer and the chunk must agree; this also rejects null_count > num_rows.
            if bitmap.null_count(num_rows) != null_count as usize {
                return Err("Chunk null count does not match the footer".to_string());
            }
            bitmap
        } else {
            Bitmap::all_valid(num_rows)
        };
        let data = VectorData::decode(buffer, ty, num_rows)?;
        Ok(Vector { data, validity })
    }
}
fn take_bytes<'a>(buffer: &mut &'a [u8], len: usize, err_msg: &str) -> Result<&'a [u8], String> {
    if buffer.len() < len {
        return Err(err_msg.to_string());
    }
    let (chunk, rest) = buffer.split_at(len);
    *buffer = rest;
    Ok(chunk)
}

pub fn write(path: &str, schema: &TableSchema, columns: &[Vector]) -> Result<(), String> {
    // Validate everything before creating the file, so bad input never leaves a partial file.
    if columns.is_empty() || columns.len() != schema.columns.len() {
        return Err(format!(
            "Schema has {} columns but {} were given",
            schema.columns.len(),
            columns.len()
        ));
    }
    let total_rows = columns[0].data.len();
    for i in 0..columns.len() {
        let column = &columns[i];
        let expected = &schema.columns[i];
        if column.data.data_type() != expected.ty {
            return Err(format!(
                "Column {} is {:?} in the schema but the data is {:?}",
                expected.name,
                expected.ty,
                column.data.data_type()
            ));
        }
        if column.data.len() != total_rows {
            return Err(format!(
                "Column {} has {} rows, expected {}",
                expected.name,
                column.data.len(),
                total_rows
            ));
        }
        // null_count_range would panic on a short bitmap; reject it here instead.
        if column.validity.words.len() != Bitmap::num_words(total_rows) {
            return Err(format!(
                "Column {} bitmap has {} words, expected {}",
                expected.name,
                column.validity.words.len(),
                Bitmap::num_words(total_rows)
            ));
        }
    }
    //- Large writes: BufWriter, or one write_all per encoded row group. Not one syscall per value.
    //- No half-written files: write to path.tmp, call sync_all(), then rename it to path.
    //   A rename is atomic, so readers never see a file without its footer. This is cheap, and it fits the write-once design.
    //- Honest benchmarks: a read benchmark straight after writing measures RAM, not disk. For cold-cache numbers,
    // drop the page cache first (sync; echo 3 | sudo tee /proc/sys/vm/drop_caches), and say which kind of number you're reporting.
    let mut buffer: Vec<u8> = MAGIC.to_vec();
    let mut rg_metas: Vec<RowGroupMeta> =
        Vec::with_capacity((total_rows + ROW_GROUP_SIZE - 1) / ROW_GROUP_SIZE);
    for start_idx in (0..total_rows).step_by(ROW_GROUP_SIZE) {
        let end_idx = std::cmp::min(start_idx + ROW_GROUP_SIZE, total_rows);
        let current_batch_count = end_idx - start_idx;
        let mut chunks_meta: Vec<ChunkMetadata> = Vec::with_capacity(columns.len()); // one per column
        let len_before_write = buffer.len();
        for column in columns {
            let offset = buffer.len() as u64;
            let null_count = column.encode_range(start_idx, current_batch_count, &mut buffer);
            let chunk_length = (buffer.len() as u64 - offset) as u32;
            let zone_map = if null_count != current_batch_count as u32 {
                Some(column.zone_map_range(start_idx, current_batch_count))
            } else {
                None
            };
            chunks_meta.push(ChunkMetadata {
                chunk_length,
                offset,
                null_count,
                zone_map,
            });
        }
        let rg_meta = RowGroupMeta {
            chunks_metadata: chunks_meta,
            num_rows: current_batch_count as u32,
            total_byte_size: (buffer.len() - len_before_write) as u64,
        };
        rg_metas.push(rg_meta);
    }
    let footer = PaxFooter::new(schema.clone(), rg_metas);
    let footer_start = buffer.len();
    footer.encode(&mut buffer)?; // validates, then appends
    let footer_len = (buffer.len() - footer_start) as u32; // measured, like chunk_length
    buffer.extend_from_slice(&footer_len.to_le_bytes());
    buffer.extend_from_slice(&MAGIC);
    let tmp_path = format!("{path}.tmp");
    let mut file = File::create(&tmp_path).map_err(|e| e.to_string())?;
    file.write_all(&buffer).map_err(|e| e.to_string())?;
    file.sync_all().map_err(|e| e.to_string())?;
    fs::rename(&tmp_path, path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_tableschema() {
        let schema = TableSchema::new(
            "users",
            vec![
                Column::new("id", DataType::Int64),
                Column::new("username", DataType::Varchar),
            ],
        );

        let mut buffer: Vec<u8> = Vec::new();
        schema.encode(&mut buffer);

        // --- Table Metadata ---
        assert_eq!(&buffer[0..4], &[5, 0, 0, 0]); // Table name length (u32)
        assert_eq!(&buffer[4..9], b"users"); // Table name string
        assert_eq!(&buffer[9..11], &[2, 0]); // Columns count (u16)

        // --- Column 1: "id" (Int64) ---
        assert_eq!(&buffer[11..15], &[2, 0, 0, 0]); // Column name length (u32)
        assert_eq!(&buffer[15..17], b"id"); // Column name string
        assert_eq!(buffer[17], 1); // DataType::Int64 (u8)

        // --- Column 2: "username" (Varchar) ---
        assert_eq!(&buffer[18..22], &[8, 0, 0, 0]); // Column name length (u32)
        assert_eq!(&buffer[22..30], b"username"); // Column name string
        assert_eq!(buffer[30], 4); // DataType::Varchar (u8)

        // Ensure we tested the entire buffer and no extra bytes were written
        assert_eq!(buffer.len(), 31);
    }

    #[test]
    fn decode_tableschema() {
        let schema = TableSchema::new(
            "users",
            vec![
                Column::new("id", DataType::Int64),
                Column::new("username", DataType::Varchar),
            ],
        );

        let mut buffer: Vec<u8> = Vec::new();
        schema.encode(&mut buffer);

        let mut bytes: &[u8] = &buffer; // decode shrinks this slice as it reads
        let decoded = TableSchema::decode(&mut bytes).unwrap();

        assert_eq!(decoded, schema);

        // decode must consume exactly what encode wrote
        assert!(bytes.is_empty());
    }

    // Encoded bytes of the "users" schema from the tests above (31 bytes).
    fn users_bytes() -> Vec<u8> {
        let schema = TableSchema::new(
            "users",
            vec![
                Column::new("id", DataType::Int64),
                Column::new("username", DataType::Varchar),
            ],
        );
        let mut buffer = Vec::new();
        schema.encode(&mut buffer);
        buffer
    }

    #[test]
    fn decode_truncated() {
        let buffer = users_bytes();
        // Cutting the buffer anywhere before the end must fail, never panic.
        for len in 0..buffer.len() {
            let mut bytes: &[u8] = &buffer[..len];
            assert!(TableSchema::decode(&mut bytes).is_err(), "len {len}");
        }
    }

    #[test]
    fn decode_unknown_type() {
        let mut buffer = users_bytes();
        buffer[17] = 9; // type byte of column "id"
        let mut bytes: &[u8] = &buffer;
        assert!(TableSchema::decode(&mut bytes).is_err());
    }

    #[test]
    fn decode_invalid_utf8() {
        let mut buffer = users_bytes();
        buffer[4] = 0xFF; // first byte of "users"; 0xFF is never valid UTF-8
        let mut bytes: &[u8] = &buffer;
        assert!(TableSchema::decode(&mut bytes).is_err());
    }

    fn users_schema() -> TableSchema {
        TableSchema::new(
            "users",
            vec![
                Column::new("id", DataType::Int64),
                Column::new("username", DataType::Varchar),
            ],
        )
    }

    // A chunk for column "id" with a zone map of 1..=100.
    fn id_chunk() -> ChunkMetadata {
        ChunkMetadata {
            chunk_length: 1024,
            offset: 0,
            null_count: 0,
            zone_map: Some(ZoneMap {
                min: Scalar::I64(1),
                max: Scalar::I64(100),
            }),
        }
    }

    // A chunk for column "username" where all 100 rows are NULL, so no zone map.
    fn username_chunk() -> ChunkMetadata {
        ChunkMetadata {
            chunk_length: 512,
            offset: 1024,
            null_count: 100,
            zone_map: None,
        }
    }

    #[test]
    fn encode_pax_footer() {
        let row_group = RowGroupMeta {
            num_rows: 100,
            total_byte_size: 1536,
            chunks_metadata: vec![id_chunk(), username_chunk()], // one chunk per column
        };
        let footer = PaxFooter::new(users_schema(), vec![row_group]);

        let mut buffer: Vec<u8> = Vec::new();
        footer.encode(&mut buffer).unwrap();

        // --- Table Schema (same 31 bytes as encode_tableschema) ---
        assert_eq!(&buffer[0..4], &[5, 0, 0, 0]); // Table name length (5)
        assert_eq!(&buffer[4..9], b"users"); // Table name string
        assert_eq!(&buffer[9..11], &[2, 0]); // Columns count (2)
        // ... column metadata up to byte 31 ...

        // --- Row groups ---
        assert_eq!(&buffer[31..35], &[1, 0, 0, 0]); // Row groups count (1)
        assert_eq!(&buffer[35..39], &[100, 0, 0, 0]); // num_rows (100)
        assert_eq!(&buffer[39..47], &[0, 6, 0, 0, 0, 0, 0, 0]); // total_byte_size (1536 = 0x0600)

        // --- Chunk 0: "id" ---
        assert_eq!(&buffer[47..51], &[0, 4, 0, 0]); // chunk_length (1024)
        assert_eq!(&buffer[51..59], &[0, 0, 0, 0, 0, 0, 0, 0]); // offset (0)
        assert_eq!(&buffer[59..63], &[0, 0, 0, 0]); // null_count (0)
        assert_eq!(buffer[63], 1); // zone map present
        assert_eq!(&buffer[64..72], &[1, 0, 0, 0, 0, 0, 0, 0]); // min (1 as i64)
        assert_eq!(&buffer[72..80], &[100, 0, 0, 0, 0, 0, 0, 0]); // max (100 as i64)

        // --- Chunk 1: "username" (all NULL) ---
        assert_eq!(&buffer[80..84], &[0, 2, 0, 0]); // chunk_length (512)
        assert_eq!(&buffer[84..92], &[0, 4, 0, 0, 0, 0, 0, 0]); // offset (1024)
        assert_eq!(&buffer[92..96], &[100, 0, 0, 0]); // null_count (100)
        assert_eq!(buffer[96], 0); // zone map absent, nothing follows

        // Ensure we tested the entire buffer: 31 schema + 4 count + 12 row group + 33 + 17 chunks
        assert_eq!(buffer.len(), 97);
    }

    #[test]
    fn encode_rejects_wrong_chunk_count() {
        let row_group = RowGroupMeta {
            num_rows: 100,
            total_byte_size: 1024,
            chunks_metadata: vec![id_chunk()], // schema has 2 columns
        };
        let footer = PaxFooter::new(users_schema(), vec![row_group]);

        let mut buffer: Vec<u8> = Vec::new();
        assert!(footer.encode(&mut buffer).is_err());
        assert!(buffer.is_empty()); // nothing written on error
    }

    #[test]
    fn encode_rejects_wrong_zone_map_type() {
        let mut bad_chunk = id_chunk();
        bad_chunk.zone_map = Some(ZoneMap {
            min: Scalar::F64(1.0), // column "id" is Int64
            max: Scalar::F64(100.0),
        });
        let row_group = RowGroupMeta {
            num_rows: 100,
            total_byte_size: 1536,
            chunks_metadata: vec![bad_chunk, username_chunk()],
        };
        let footer = PaxFooter::new(users_schema(), vec![row_group]);

        let mut buffer: Vec<u8> = Vec::new();
        assert!(footer.encode(&mut buffer).is_err());
    }

    // The footer from encode_pax_footer: 1 row group, "id" with a zone map, "username" all NULL.
    fn users_footer_bytes() -> Vec<u8> {
        let row_group = RowGroupMeta {
            num_rows: 100,
            total_byte_size: 1536,
            chunks_metadata: vec![id_chunk(), username_chunk()],
        };
        let footer = PaxFooter::new(users_schema(), vec![row_group]);
        let mut buffer = Vec::new();
        footer.encode(&mut buffer).unwrap();
        buffer
    }

    #[test]
    fn decode_pax_footer() {
        let row_group = RowGroupMeta {
            num_rows: 100,
            total_byte_size: 1536,
            chunks_metadata: vec![id_chunk(), username_chunk()],
        };
        let footer = PaxFooter::new(users_schema(), vec![row_group]);

        let mut buffer: Vec<u8> = Vec::new();
        footer.encode(&mut buffer).unwrap();

        let mut bytes: &[u8] = &buffer;
        let decoded = PaxFooter::decode(&mut bytes).unwrap();

        assert_eq!(decoded, footer);
        assert!(bytes.is_empty());
    }

    #[test]
    fn decode_scalar_every_type() {
        let cases = vec![
            (Scalar::I64(-42), DataType::Int64),
            (Scalar::F64(0.07), DataType::Float64),
            (Scalar::Date(9568), DataType::Date), // 1996-03-13
            (Scalar::String("AIR".to_string()), DataType::Varchar),
        ];
        for (value, ty) in cases {
            let mut buffer: Vec<u8> = Vec::new();
            value.encode(&mut buffer);

            let mut bytes: &[u8] = &buffer;
            let decoded = Scalar::decode(&mut bytes, ty).unwrap();

            assert_eq!(decoded, value);
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn decode_footer_truncated() {
        let buffer = users_footer_bytes();
        // Cutting the footer anywhere before the end must fail, never panic.
        for len in 0..buffer.len() {
            let mut bytes: &[u8] = &buffer[..len];
            assert!(PaxFooter::decode(&mut bytes).is_err(), "len {len}");
        }
    }

    #[test]
    fn decode_footer_bad_zone_map_flag() {
        let mut buffer = users_footer_bytes();
        buffer[63] = 2; // zone map flag of chunk "id"; only 0 or 1 are valid
        let mut bytes: &[u8] = &buffer;
        assert!(PaxFooter::decode(&mut bytes).is_err());
    }

    #[test]
    fn decode_footer_trailing_bytes() {
        let mut buffer = users_footer_bytes();
        buffer.push(0); // one byte too many
        let mut bytes: &[u8] = &buffer;
        assert!(PaxFooter::decode(&mut bytes).is_err());
    }

    #[test]
    fn decode_footer_huge_row_group_count() {
        let mut buffer = users_footer_bytes();
        buffer[31..35].copy_from_slice(&u32::MAX.to_le_bytes()); // row group count
        let mut bytes: &[u8] = &buffer;
        // Must fail with "unexpected end", not try to allocate ~4 billion row groups.
        assert!(PaxFooter::decode(&mut bytes).is_err());
    }

    #[test]
    fn decode_vectordata_fixed_width() {
        let cases = vec![
            (VectorData::Int64(vec![1, -2, i64::MAX]), DataType::Int64),
            (
                VectorData::Float64(vec![0.04, -1.5, 0.0]),
                DataType::Float64,
            ),
            (VectorData::Date(vec![9568, 0, -1]), DataType::Date), // 1996-03-13, 1970-01-01, 1969-12-31
        ];
        for (values, ty) in cases {
            let mut buffer: Vec<u8> = Vec::new();
            values.encode(&mut buffer);

            let mut bytes: &[u8] = &buffer;
            let decoded = VectorData::decode(&mut bytes, ty, 3).unwrap();

            assert_eq!(decoded, values);
            assert!(bytes.is_empty());
        }
    }

    #[test]
    fn decode_vectordata_too_few_bytes() {
        let mut buffer: Vec<u8> = Vec::new();
        VectorData::Int64(vec![1, 2, 3]).encode(&mut buffer);

        // 24 bytes hold 3 values; asking for 4 must fail, not read past the end.
        let mut bytes: &[u8] = &buffer;
        assert!(VectorData::decode(&mut bytes, DataType::Int64, 4).is_err());
    }

    #[test]
    fn decode_vectordata_huge_num_rows() {
        let mut buffer: Vec<u8> = Vec::new();
        VectorData::Int64(vec![1, 2, 3]).encode(&mut buffer);

        // A corrupted row count must fail cleanly, not try to reserve ~34 GB.
        let mut bytes: &[u8] = &buffer;
        assert!(VectorData::decode(&mut bytes, DataType::Int64, u32::MAX as usize).is_err());
    }

    fn varchar_bytes(values: &[&str]) -> Vec<u8> {
        let strings: Vec<String> = values.iter().map(|s| s.to_string()).collect();
        let mut buffer = Vec::new();
        VectorData::Varchar(strings).encode(&mut buffer);
        buffer
    }

    #[test]
    fn decode_vectordata_varchar() {
        // "" is how a NULL slot looks; "café" has 4 chars but 5 bytes
        let values = VectorData::Varchar(vec![
            "AIR".to_string(),
            "".to_string(),
            "TRUCK".to_string(),
            "café".to_string(),
        ]);
        let mut buffer: Vec<u8> = Vec::new();
        values.encode(&mut buffer);
        assert_eq!(buffer.len(), 5 * 4 + 13); // offsets [0, 3, 3, 8, 13] + 13 data bytes

        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 4).unwrap();

        assert_eq!(decoded, values);
        assert!(bytes.is_empty()); // data area consumed too, not just offsets
    }

    #[test]
    fn decode_vectordata_varchar_zero_rows() {
        let buffer = varchar_bytes(&[]); // just the single offset 0
        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 0).unwrap();
        assert_eq!(decoded, VectorData::Varchar(vec![]));
        assert!(bytes.is_empty());
    }

    #[test]
    fn decode_vectordata_varchar_truncated() {
        let buffer = varchar_bytes(&["AIR", "TRUCK"]);
        // Cutting anywhere before the end must fail, never panic.
        for len in 0..buffer.len() {
            let mut bytes: &[u8] = &buffer[..len];
            assert!(
                VectorData::decode(&mut bytes, DataType::Varchar, 2).is_err(),
                "len {len}"
            );
        }
    }

    #[test]
    fn decode_vectordata_varchar_bad_offsets() {
        // offsets [0, 3, 8] + "AIRTRUCK"
        let mut first_not_zero = varchar_bytes(&["AIR", "TRUCK"]);
        first_not_zero[0] = 1;
        let mut bytes: &[u8] = &first_not_zero;
        assert!(VectorData::decode(&mut bytes, DataType::Varchar, 2).is_err());

        let mut decreasing = varchar_bytes(&["AIR", "TRUCK"]);
        decreasing[4] = 9; // offsets become [0, 9, 8]
        let mut bytes: &[u8] = &decreasing;
        assert!(VectorData::decode(&mut bytes, DataType::Varchar, 2).is_err());
    }

    #[test]
    fn decode_vectordata_varchar_invalid_utf8() {
        let mut buffer = varchar_bytes(&["AIR"]);
        buffer[8] = 0xFF; // first data byte, after offsets [0, 3]
        let mut bytes: &[u8] = &buffer;
        assert!(VectorData::decode(&mut bytes, DataType::Varchar, 1).is_err());
    }

    #[test]
    fn decode_bitmap() {
        // 100 rows -> 2 words. NULLs on both sides of the word boundary (63 | 64) and at the ends.
        let mut bitmap = Bitmap::all_valid(100);
        for i in [0, 63, 64, 99] {
            bitmap.set_null(i);
        }

        let mut buffer: Vec<u8> = Vec::new();
        bitmap.encode(&mut buffer);
        buffer.extend_from_slice(&[7, 7, 7]); // pretend the values follow the bitmap
        assert_eq!(buffer.len(), 2 * 8 + 3);

        let mut bytes: &[u8] = &buffer;
        let decoded = Bitmap::decode(&mut bytes, 100).unwrap();

        assert_eq!(decoded, bitmap);
        assert!(!decoded.is_valid(64));
        assert!(decoded.is_valid(65));
        assert_eq!(bytes, &[7, 7, 7]); // only the bitmap's bytes were taken
    }

    #[test]
    fn decode_bitmap_too_few_bytes() {
        let mut buffer: Vec<u8> = Vec::new();
        Bitmap::all_valid(64).encode(&mut buffer); // 1 word

        // 65 rows need 2 words
        let mut bytes: &[u8] = &buffer;
        assert!(Bitmap::decode(&mut bytes, 65).is_err());
    }

    #[test]
    fn decode_bitmap_huge_num_rows() {
        let mut buffer: Vec<u8> = Vec::new();
        Bitmap::all_valid(64).encode(&mut buffer);

        // A corrupted row count must fail cleanly, not reserve memory for it.
        let mut bytes: &[u8] = &buffer;
        assert!(Bitmap::decode(&mut bytes, u32::MAX as usize).is_err());
    }

    // 100 rows of i64 with NULLs at 0, 63 and 99 (NULL slots hold 0).
    fn vector_with_nulls() -> Vector {
        let mut validity = Bitmap::all_valid(100);
        for i in [0, 63, 99] {
            validity.set_null(i);
        }
        let values: Vec<i64> = (0..100)
            .map(|i| if validity.is_valid(i) { i as i64 } else { 0 })
            .collect();
        Vector {
            data: VectorData::Int64(values),
            validity,
        }
    }

    #[test]
    fn vector_round_trip_with_nulls() {
        let vector = vector_with_nulls();
        let mut buffer: Vec<u8> = Vec::new();
        let null_count = vector.encode(&mut buffer);
        assert_eq!(null_count, 3);
        assert_eq!(buffer.len(), 16 + 800); // 2 bitmap words, then 100 values

        let mut bytes: &[u8] = &buffer;
        let decoded = Vector::decode(&mut bytes, DataType::Int64, 100, null_count).unwrap();
        assert_eq!(decoded, vector);
        assert!(bytes.is_empty());
    }

    #[test]
    fn vector_round_trip_no_nulls() {
        let vector = Vector {
            data: VectorData::Varchar(vec!["AIR".to_string(), "".to_string(), "café".to_string()]),
            validity: Bitmap::all_valid(3),
        };
        let mut buffer: Vec<u8> = Vec::new();
        let null_count = vector.encode(&mut buffer);
        assert_eq!(null_count, 0);
        assert_eq!(buffer.len(), 16 + 8); // no bitmap: 4 offsets, then 8 string bytes

        let mut bytes: &[u8] = &buffer;
        let decoded = Vector::decode(&mut bytes, DataType::Varchar, 3, null_count).unwrap();
        assert_eq!(decoded, vector);
        assert!(bytes.is_empty());
    }

    #[test]
    fn vector_decode_null_count_mismatch() {
        let mut buffer: Vec<u8> = Vec::new();
        vector_with_nulls().encode(&mut buffer);

        // The bitmap has 3 NULLs but the footer claims 5.
        let mut bytes: &[u8] = &buffer;
        assert!(Vector::decode(&mut bytes, DataType::Int64, 100, 5).is_err());
    }

    #[test]
    fn vector_decode_truncated() {
        let mut buffer: Vec<u8> = Vec::new();
        vector_with_nulls().encode(&mut buffer);
        buffer.truncate(buffer.len() - 1);

        let mut bytes: &[u8] = &buffer;
        assert!(Vector::decode(&mut bytes, DataType::Int64, 100, 3).is_err());
    }

    // 150 rows of i64 with NULLs at 3, 70 and 149 (NULL slots hold 0).
    fn int_vector_150() -> Vector {
        let mut validity = Bitmap::all_valid(150);
        for i in [3, 70, 149] {
            validity.set_null(i);
        }
        let values: Vec<i64> = (0..150)
            .map(|i| if validity.is_valid(i) { i as i64 } else { 0 })
            .collect();
        Vector {
            data: VectorData::Int64(values),
            validity,
        }
    }

    // The same rows copied into their own Vector, the slow way the range encode avoids.
    fn copy_rows(vector: &Vector, start: usize, num_rows: usize) -> Vector {
        let mut validity = Bitmap::all_valid(num_rows);
        for i in 0..num_rows {
            if !vector.validity.is_valid(start + i) {
                validity.set_null(i);
            }
        }
        Vector {
            data: vector.data.slice(start, num_rows),
            validity,
        }
    }

    #[test]
    fn encode_range_matches_encoding_a_copy() {
        let vector = int_vector_150();
        // Rows 64..150: a word-aligned start and a partial last word, like a final row group.
        let mut ranged: Vec<u8> = Vec::new();
        let null_count = vector.encode_range(64, 86, &mut ranged);
        assert_eq!(null_count, 2); // rows 70 and 149

        let mut copied: Vec<u8> = Vec::new();
        copy_rows(&vector, 64, 86).encode(&mut copied);
        assert_eq!(ranged, copied);

        let mut bytes: &[u8] = &ranged;
        let decoded = Vector::decode(&mut bytes, DataType::Int64, 86, null_count).unwrap();
        assert_eq!(decoded, copy_rows(&vector, 64, 86));
    }

    #[test]
    fn encode_range_without_nulls_skips_bitmap() {
        let vector = int_vector_150();
        // Rows 0..64 hold only the NULL at row 3; rows 128..148 hold none.
        let mut buffer: Vec<u8> = Vec::new();
        assert_eq!(vector.encode_range(128, 20, &mut buffer), 0);
        assert_eq!(buffer.len(), 20 * 8); // values only
    }

    #[test]
    fn encode_range_varchar_offsets_restart() {
        let names = ["a", "bb", "ccc", "dddd"].map(|s| s.to_string()).to_vec();
        let mut strings = Vec::new();
        for i in 0..70 {
            strings.push(names[i % 4].clone());
        }
        let vector = Vector {
            data: VectorData::Varchar(strings),
            validity: Bitmap::all_valid(70),
        };

        let mut ranged: Vec<u8> = Vec::new();
        vector.encode_range(64, 6, &mut ranged);
        let mut copied: Vec<u8> = Vec::new();
        copy_rows(&vector, 64, 6).encode(&mut copied);
        assert_eq!(ranged, copied); // first offset is 0, not the bytes before row 64
    }

    #[test]
    #[should_panic(expected = "multiple of 64")]
    fn encode_range_rejects_unaligned_start() {
        int_vector_150().encode_range(10, 5, &mut Vec::new());
    }

    #[test]
    fn zone_map_range_skips_nulls() {
        // Rows 64..150 hold 64..148; the NULL at 149 holds 0, which must not become the min.
        let zone_map = int_vector_150().zone_map_range(64, 86);
        assert_eq!(
            zone_map,
            ZoneMap {
                min: Scalar::I64(64),
                max: Scalar::I64(148)
            }
        );
    }

    #[test]
    fn zone_map_range_first_row_null() {
        // Row 3 is NULL: the scan must start from the first non-NULL row, 4.
        let zone_map = int_vector_150().zone_map_range(0, 64).min;
        assert_eq!(zone_map, Scalar::I64(0));
        let mut validity = Bitmap::all_valid(3);
        validity.set_null(0);
        let vector = Vector {
            data: VectorData::Date(vec![-5, 10, 7]),
            validity,
        };
        assert_eq!(
            vector.zone_map_range(0, 3),
            ZoneMap {
                min: Scalar::Date(7),
                max: Scalar::Date(10)
            }
        );
    }

    #[test]
    fn zone_map_range_float_ignores_nan() {
        let vector = Vector {
            data: VectorData::Float64(vec![f64::NAN, 2.5, -1.0, f64::NAN]),
            validity: Bitmap::all_valid(4),
        };
        assert_eq!(
            vector.zone_map_range(0, 4),
            ZoneMap {
                min: Scalar::F64(-1.0),
                max: Scalar::F64(2.5)
            }
        );
    }

    #[test]
    fn zone_map_range_varchar() {
        let vector = Vector {
            data: VectorData::Varchar(["TRUCK", "AIR", "", "café"].map(|s| s.to_string()).to_vec()),
            validity: Bitmap::all_valid(4),
        };
        let zone_map = vector.zone_map_range(0, 4);
        assert_eq!(zone_map.min, Scalar::String("".to_string()));
        assert_eq!(zone_map.max, Scalar::String("café".to_string())); // lowercase sorts after uppercase
    }

    #[test]
    #[should_panic(expected = "all NULL")]
    fn zone_map_range_all_null_panics() {
        let mut validity = Bitmap::all_valid(2);
        validity.set_null(0);
        validity.set_null(1);
        Vector {
            data: VectorData::Int64(vec![0, 0]),
            validity,
        }
        .zone_map_range(0, 2);
    }

    fn int_schema(names: &[&str]) -> TableSchema {
        TableSchema::new(
            "t",
            names
                .iter()
                .map(|n| Column::new(n, DataType::Int64))
                .collect(),
        )
    }

    fn int_column(values: Vec<i64>) -> Vector {
        let validity = Bitmap::all_valid(values.len());
        Vector {
            data: VectorData::Int64(values),
            validity,
        }
    }

    #[test]
    fn write_rejects_wrong_column_count() {
        let result = write(
            "unused.pax",
            &int_schema(&["a", "b"]),
            &[int_column(vec![1])],
        );
        assert!(result.is_err());
    }

    #[test]
    fn write_rejects_wrong_type() {
        let column = Vector {
            data: VectorData::Date(vec![1]),
            validity: Bitmap::all_valid(1),
        };
        assert!(write("unused.pax", &int_schema(&["a"]), &[column]).is_err());
    }

    #[test]
    fn write_rejects_different_lengths() {
        let columns = [int_column(vec![1, 2]), int_column(vec![1])];
        assert!(write("unused.pax", &int_schema(&["a", "b"]), &columns).is_err());
    }

    #[test]
    fn write_file_layout() {
        let path = std::env::temp_dir().join("olap_write_file_layout.pax");
        let path = path.to_str().unwrap();
        // 70,000 rows: one full row group and one of 4,464 rows.
        let columns = [int_column((0..70_000).collect())];
        let schema = int_schema(&["a"]);
        write(path, &schema, &columns).unwrap();

        let file = std::fs::read(path).unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(&file[..4], &MAGIC);
        assert_eq!(&file[file.len() - 4..], &MAGIC);

        // Tail: [footer][u32 footer length][magic]
        let len_at = file.len() - 8;
        let footer_len = u32::from_le_bytes(file[len_at..len_at + 4].try_into().unwrap()) as usize;
        let mut footer_bytes: &[u8] = &file[len_at - footer_len..len_at];
        let footer = PaxFooter::decode(&mut footer_bytes).unwrap();

        assert_eq!(footer.scheme, schema);
        let groups = &footer.row_groups_metadata;
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].num_rows, 4_464);
        let chunk = &groups[1].chunks_metadata[0];
        assert_eq!(chunk.offset, 4 + 65_536 * 8); // right after group 0
        assert_eq!(chunk.chunk_length, 4_464 * 8);
        assert_eq!(
            chunk.zone_map,
            Some(ZoneMap {
                min: Scalar::I64(65_536),
                max: Scalar::I64(69_999)
            })
        );

        // The chunk's bytes decode back to the original rows.
        let start = chunk.offset as usize;
        let mut bytes: &[u8] = &file[start..start + chunk.chunk_length as usize];
        let decoded = Vector::decode(&mut bytes, DataType::Int64, 4_464, chunk.null_count).unwrap();
        assert_eq!(decoded.data, VectorData::Int64((65_536..70_000).collect()));
    }
}
