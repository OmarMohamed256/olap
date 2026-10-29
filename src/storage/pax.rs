const MAGIC: [u8; 4] = *b"PAX1";
// Rows per row group: 32 DataChunks of 2048. A multiple of 64, so each group's bitmap starts on a word boundary.
const ROW_GROUP_SIZE: usize = 65_536;

use crate::{
    catalog::{Column, DataType, TableSchema},
    exec::exec::{Bitmap, ColumnRange, Scalar, StringVector, TableSource, Vector, VectorData},
};
use std::cmp::Ordering;
use std::collections::{HashMap, hash_map::Entry};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write}; // traits that add read_exact, seek and write_all to File
use std::ops::Bound;
use std::os::unix::fs::FileExt; // adds read_exact_at: read at an offset without moving the cursor

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

// Scalar's PAX byte format; the type itself lives in exec.
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

#[repr(u8)]
pub enum EncodingTag {
    Plain = 0,
    Dictionary = 1,
    FrameOfReference = 2,
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
                // Frame of reference: store min once, then each value as (value - min) in bit_width bits.
                // wrapping_sub as u64 gives the true distance even when it doesn't fit in an i64.
                let min = v.iter().copied().min().unwrap_or(0);
                let max = v.iter().copied().max().unwrap_or(0);
                let spread = max.wrapping_sub(min) as u64;
                let bit_width = (u64::BITS - spread.leading_zeros()) as u8; // 0 when all values are equal

                // tag + values as i64
                let plain_byte_size = 1 + 8 * num_rows;
                let packed_bits = num_rows * bit_width as usize;
                // tag + min + bit_width + packed deltas
                let frame_of_reference_byte_size = 1 + 8 + 1 + packed_bits.div_ceil(8);
                if frame_of_reference_byte_size >= plain_byte_size {
                    buffer.reserve(plain_byte_size);
                    buffer.push(EncodingTag::Plain as u8);
                    for &val in v {
                        buffer.extend_from_slice(&val.to_le_bytes());
                    }
                } else {
                    buffer.reserve(frame_of_reference_byte_size);
                    buffer.push(EncodingTag::FrameOfReference as u8);
                    buffer.extend_from_slice(&min.to_le_bytes());
                    buffer.push(bit_width);
                    // u128: up to 7 waiting bits + a 64-bit delta = 71 bits.
                    let mut acc: u128 = 0;
                    let mut bits_in_acc: u32 = 0;
                    for &val in v {
                        let delta = val.wrapping_sub(min) as u64;
                        acc |= (delta as u128) << bits_in_acc;
                        bits_in_acc += bit_width as u32;
                        while bits_in_acc >= 8 {
                            buffer.push(acc as u8); // `as u8` keeps the low 8 bits
                            acc >>= 8;
                            bits_in_acc -= 8;
                        }
                    }
                    // Last partial byte; its unused high bits are 0 padding.
                    if bits_in_acc > 0 {
                        buffer.push(acc as u8);
                    }
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
            VectorData::Varchar(string_vector) => {
                let mut codes_of: HashMap<&str, u32> = HashMap::new();
                let offsets = string_vector.offsets();
                let first = offsets[start];
                let last = offsets[start + num_rows];
                // Both sizes include the 1-byte tag.
                // plain:      tag + (num_rows + 1) offsets + string bytes
                // dictionary: tag + num_distinct (u32) + (num_distinct + 1) offsets
                //             + distinct bytes + num_rows codes of code_width(num_distinct) bytes
                let total_plain_bytes_len = 1 + (num_rows + 1) * 4 + (last - first) as usize;
                let dict_bytes_len = |distinct: &StringVector, width: usize| {
                    1 + 4 + (distinct.len() + 1) * 4 + distinct.data().len() + num_rows * width
                };
                let mut distinct_values = StringVector::new();
                let mut codes: Vec<u32> = Vec::with_capacity(num_rows);
                let mut use_plain = false;

                for i in start..start + num_rows {
                    let s = string_vector.get(i);
                    match codes_of.entry(s) {
                        Entry::Occupied(e) => codes.push(*e.get()),
                        Entry::Vacant(e) => {
                            let code = distinct_values.len() as u32;
                            codes.push(code);
                            e.insert(code);
                            distinct_values.push(s);
                            // Early exit: the code width is only known at the end, so assume the
                            // smallest (1 byte). Even then the dictionary is too big, and it only
                            // grows from here.
                            if dict_bytes_len(&distinct_values, 1) >= total_plain_bytes_len {
                                use_plain = true;
                                break;
                            }
                        }
                    }
                }
                let width = code_width(distinct_values.len());
                let total_dict_bytes_len = dict_bytes_len(&distinct_values, width);

                if use_plain || total_dict_bytes_len >= total_plain_bytes_len {
                    // plain normal encoding
                    buffer.reserve(total_plain_bytes_len);
                    buffer.push(EncodingTag::Plain as u8);
                    for &o in &offsets[start..=start + num_rows] {
                        buffer.extend_from_slice(&(o - first).to_le_bytes());
                    }
                    let data = &string_vector.data()[first as usize..last as usize];
                    buffer.extend_from_slice(data.as_bytes())
                } else {
                    // dictionary encoding
                    buffer.reserve(total_dict_bytes_len);
                    buffer.push(EncodingTag::Dictionary as u8);
                    // num_distinct = values, not offsets; the reader reads num_distinct + 1 offsets.
                    buffer.extend_from_slice(&(distinct_values.len() as u32).to_le_bytes());

                    for &o in distinct_values.offsets() {
                        buffer.extend_from_slice(&o.to_le_bytes());
                    }
                    buffer.extend_from_slice(distinct_values.data().as_bytes());

                    // codes, each `width` bytes: the low bytes of the u32, little endian
                    for code in codes {
                        buffer.extend_from_slice(&code.to_le_bytes()[..width]);
                    }
                }
            }
        }
    }
    pub fn decode(buffer: &mut &[u8], ty: DataType, num_rows: usize) -> Result<Self, String> {
        match ty {
            DataType::Int64 => {
                let encoding_tag = take_bytes(buffer, 1, "Unexpected end reading encoding tag")?[0];
                match encoding_tag {
                    0 => {
                        let bytes = take_bytes(buffer, 8 * num_rows, "Unexpected end reading i64 values")?;
                        let mut values = Vec::with_capacity(num_rows); // safe: take_bytes already proved the bytes exist
                        for piece in bytes.chunks_exact(8) {
                            values.push(i64::from_le_bytes(piece.try_into().unwrap()));
                        }
                        Ok(VectorData::Int64(values))
                    }
                    // TODO: 2 => FrameOfReference
                    tag => Err(format!("Corrupted data: unknown encoding tag {tag}")),
                }
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
                // The first byte says which layout follows (EncodingTag).
                let encoding_tag = take_bytes(buffer, 1, "Unexpected end reading encoding tag")?[0];
                match encoding_tag {
                    0 => {
                        // Plain
                        // 1. Offsets: num_rows + 1 of them; string i is data[offsets[i]..offsets[i + 1]]
                        let offset_bytes = take_bytes(
                            buffer,
                            4 * (num_rows + 1),
                            "Unexpected end reading string offsets",
                        )?;
                        // ALLOC: one 256 KB Vec<u32> per chunk (same size as on disk); it moves into the
                        // StringVector, no conversion and no per-row allocation.
                        let mut offsets: Vec<u32> = Vec::with_capacity(num_rows + 1);
                        for piece in offset_bytes.chunks_exact(4) {
                            offsets.push(u32::from_le_bytes(piece.try_into().unwrap()));
                        }

                        // 2. Data area: the last offset is its total length. Not checked yet: a huge value
                        // makes take_bytes fail, a wrong small one is caught by from_parts below.
                        let data_len = offsets[num_rows];
                        let data = take_bytes(
                            buffer,
                            data_len as usize,
                            "Unexpected end reading string data",
                        )?;

                        // 3. Check UTF-8 once for the whole data area, then copy it into one String.
                        // ALLOC: one String of data_len bytes per chunk, instead of one per row.
                        let text = std::str::from_utf8(data)
                            .map_err(|_| "Invalid UTF-8 in string value".to_string())?
                            .to_string();

                        // 4. from_parts checks the offsets (start at 0, never decrease, end at data_len,
                        // on character boundaries) and returns Err on corrupt data instead of panicking.
                        Ok(VectorData::Varchar(StringVector::from_parts(
                            offsets, text,
                        )?))
                    }
                    1 => {
                        // Dictionary: num_distinct, the distinct values (same layout as plain),
                        // then one code per row of code_width(num_distinct) bytes.
                        let num_distinct_bytes =
                            take_bytes(buffer, 4, "Unexpected end reading num of distinct")?;
                        let num_distinct =
                            u32::from_le_bytes(num_distinct_bytes.try_into().unwrap());
                        let mut distinct_offsets: Vec<u32> =
                            Vec::with_capacity(num_distinct as usize + 1);
                        let distinct_offsets_bytes = take_bytes(
                            buffer,
                            4 * (num_distinct as usize + 1),
                            "Unexpected end reading distinct offsets",
                        )?;
                        for piece in distinct_offsets_bytes.chunks_exact(4) {
                            distinct_offsets.push(u32::from_le_bytes(piece.try_into().unwrap()));
                        }
                        let data_len = distinct_offsets[num_distinct as usize];
                        let data = take_bytes(
                            buffer,
                            data_len as usize,
                            "Unexpected end reading string data",
                        )?;

                        let dict_text = std::str::from_utf8(data)
                            .map_err(|_| "Invalid UTF-8 in string value".to_string())?
                            .to_string();

                        let distinct_values =
                            StringVector::from_parts(distinct_offsets, dict_text)?;

                        // The code width isn't stored: it follows from num_distinct.
                        let width = code_width(num_distinct as usize);
                        let codes_bytes =
                            take_bytes(buffer, num_rows * width, "Unexpected end reading codes")?;
                        // ALLOC: one Vec<u32> of codes per chunk. Matching on the width once,
                        // outside the loop, keeps each loop a fixed-size read.
                        let codes: Vec<u32> = match width {
                            1 => codes_bytes.iter().map(|&b| b as u32).collect(),
                            2 => codes_bytes
                                .chunks_exact(2)
                                .map(|p| u16::from_le_bytes(p.try_into().unwrap()) as u32)
                                .collect(),
                            _ => codes_bytes
                                .chunks_exact(4)
                                .map(|p| u32::from_le_bytes(p.try_into().unwrap()))
                                .collect(),
                        };

                        // Pass 1: check every code and build the output offsets.
                        // A code past the distinct values would index out of bounds.
                        let distinct_offsets = distinct_values.offsets();
                        let mut offsets: Vec<u32> = Vec::with_capacity(num_rows + 1);
                        let mut end: usize = 0;
                        offsets.push(0);
                        for &code in &codes {
                            let code = code as usize;
                            if code >= distinct_values.len() {
                                return Err(format!(
                                    "Corrupted data: code {code} but only {num_distinct} distinct values"
                                ));
                            }
                            end += (distinct_offsets[code + 1] - distinct_offsets[code]) as usize;
                            offsets.push(u32::try_from(end).map_err(|_| {
                                "Corrupted data: decoded strings exceed 4 GB".to_string()
                            })?);
                        }

                        // Pass 2: copy each row's bytes out of the distinct values. Bytes, not
                        // &str: slicing a &str checks character boundaries on every row.
                        // ALLOC: one buffer of exactly `end` bytes per chunk.
                        let distinct_bytes = distinct_values.data().as_bytes();
                        let mut data: Vec<u8> = Vec::with_capacity(end);
                        for &code in &codes {
                            let code = code as usize;
                            let range = distinct_offsets[code] as usize
                                ..distinct_offsets[code + 1] as usize;
                            data.extend_from_slice(&distinct_bytes[range]);
                        }
                        // Pieces of valid UTF-8 joined at their boundaries stay valid, but checking
                        // is one fast pass and keeps this safe.
                        let text = String::from_utf8(data)
                            .map_err(|_| "Invalid UTF-8 in string value".to_string())?;
                        Ok(VectorData::Varchar(StringVector::from_parts(
                            offsets, text,
                        )?))
                    }
                    tag => Err(format!("Corrupted data: unknown encoding tag {tag}")),
                }
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
                let mut min = v.get(first);
                let mut max = v.get(first);
                for i in first + 1..end {
                    if self.validity.is_valid(i) {
                        if v.get(i) < min {
                            min = v.get(i);
                        }
                        if v.get(i) > max {
                            max = v.get(i);
                        }
                    }
                }
                ZoneMap {
                    min: Scalar::String(min.to_string()),
                    max: Scalar::String(max.to_string()),
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
            Bitmap::all_valid(num_rows) // ALLOC: 8 KB per chunk, filled with 1s; small
        };
        let data = VectorData::decode(buffer, ty, num_rows)?;
        Ok(Vector { data, validity })
    }
}
// Bytes per dictionary code: the smallest of 1, 2 or 4 that can hold codes 0..num_distinct.
// A row group has at most 65,536 rows, so 4 is only reached by encode() on a bigger Vector.
fn code_width(num_distinct: usize) -> usize {
    if num_distinct <= 1 << 8 {
        1
    } else if num_distinct <= 1 << 16 {
        2
    } else {
        4
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

// An open PAX file: the file handle plus its decoded footer. Fields are private so
// callers can't move the file cursor or swap the footer; read through the methods.
pub struct PaxFile {
    file: File,
    footer: PaxFooter,
}

impl PaxFile {
    pub fn open(path: &str) -> Result<PaxFile, String> {
        let mut file = File::open(path).map_err(|e| e.to_string())?;
        let file_len = file.metadata().map_err(|e| e.to_string())?.len();
        // Smallest possible file: leading magic (4) + footer length (4) + trailing magic (4).
        if file_len < 12 {
            return Err(format!(
                "file is {file_len} bytes, too small for a PAX file"
            ));
        }

        // Tail: [u32 footer length][MAGIC]
        let mut tail = [0u8; 8];
        file.seek(SeekFrom::End(-8)).map_err(|e| e.to_string())?;
        file.read_exact(&mut tail).map_err(|e| e.to_string())?;

        let mut start_magic = [0u8; 4];
        file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
        file.read_exact(&mut start_magic)
            .map_err(|e| e.to_string())?;

        if tail[4..8] != MAGIC || start_magic != MAGIC {
            return Err("file is corrupted".to_string());
        }
        let footer_len = u32::from_le_bytes(tail[0..4].try_into().unwrap());

        if footer_len as u64 > file_len - 12 {
            return Err(format!(
                "footer length {footer_len} does not fit in a {file_len}-byte file"
            ));
        }

        file.seek(SeekFrom::Start(file_len - 8 - (footer_len as u64)))
            .map_err(|e| e.to_string())?;
        let mut footer_buffer = vec![0u8; footer_len as usize];
        file.read_exact(&mut footer_buffer)
            .map_err(|e| e.to_string())?;
        let mut slice: &[u8] = &footer_buffer;
        let footer = PaxFooter::decode(&mut slice)?;
        if !slice.is_empty() {
            return Err("failure decoding the footer".to_string());
        }
        Ok(PaxFile { file, footer })
    }

    pub fn footer(&self) -> &PaxFooter {
        &self.footer
    }

    // Bytes on disk of the given columns over all row groups: what a scan of them reads.
    pub fn columns_bytes(&self, columns: &[usize]) -> u64 {
        let mut total = 0;
        for group in &self.footer.row_groups_metadata {
            for &column in columns {
                total += group.chunks_metadata[column].chunk_length as u64;
            }
        }
        total
    }

    pub fn read_chunk(&self, group: usize, column: usize) -> Result<Vector, String> {
        // Every error names the chunk, so a failure in a 160-chunk scan says which one.
        let context = |e: String| format!("reading row group {group} column {column}: {e}");

        let rg_metadata = self
            .footer
            .row_groups_metadata
            .get(group)
            .ok_or_else(|| context("row group out of range".to_string()))?;
        let chunk_metadata = rg_metadata
            .chunks_metadata
            .get(column)
            .ok_or_else(|| context("column out of range".to_string()))?;
        // The footer decoder keeps chunks_metadata and the schema the same length,
        // so this index is in range once the one above is.
        let ty = self.footer.scheme.columns[column].ty;

        // ALLOC: one buffer per chunk (~0.3–1 MB), zeroed and then overwritten right away by the read,
        // freed at the end of this function. Large, so glibc uses mmap/munmap: page faults on every
        // scan (with the decoded Vecs: ~840 per run of 7,10,11,12, ~39% of its time in the kernel).
        // Reusing one buffer avoids it.
        let mut chunk_buffer = vec![0u8; chunk_metadata.chunk_length as usize];
        self.file
            .read_exact_at(&mut chunk_buffer, chunk_metadata.offset)
            .map_err(|e| context(e.to_string()))?;

        let mut slice: &[u8] = &chunk_buffer;
        let vector = Vector::decode(
            &mut slice,
            ty,
            rg_metadata.num_rows as usize,
            chunk_metadata.null_count,
        )
        .map_err(context)?;
        // chunk_length and the data must agree: leftover bytes mean one of them is wrong.
        if !slice.is_empty() {
            return Err(context(format!(
                "{} bytes left over after decoding",
                slice.len()
            )));
        }
        Ok(vector)
    }
}

// Lets Scan read a PAX file. Everything except read_row_group comes from the footer, already in memory.
impl TableSource for PaxFile {
    // How many row groups the footer lists.
    fn num_row_groups(&self) -> usize {
        self.footer.row_groups_metadata.len()
    }

    // Rows in group `group`, from the footer (u32 on disk, usize in memory).
    // An out-of-range group is a caller bug, so indexing may panic.
    fn row_group_num_rows(&self, group: usize) -> usize {
        self.footer.row_groups_metadata[group].num_rows as usize
    }

    // One read_chunk per requested column, in the order asked; columns not listed are never read.
    // read_chunk already checks the indexes and adds context to errors.
    fn read_row_group(&self, group: usize, columns: &[usize]) -> Result<Vec<Vector>, String> {
        let group_num = self.num_row_groups();
        if group >= group_num {
            return Err(format!(
                "row group {group} out of range: file has {group_num}"
            ));
        }
        columns
            .iter()
            .map(|&column| self.read_chunk(group, column))
            .collect()
    }

    // False only when a zone map proves no row of the group can pass some range. Anything
    // uncertain (no answer from compare, unknown column) counts as "may match": skipping
    // wrongly loses rows, keeping wrongly only costs a read.
    fn may_match(&self, rg: usize, pushed_filters: &[ColumnRange]) -> bool {
        let chunks = &self.footer.row_groups_metadata[rg].chunks_metadata;
        for range in pushed_filters {
            let Some(chunk) = chunks.get(range.column) else {
                continue; // unknown column: can't tell
            };
            // All NULL: a comparison with NULL is never true, so no row can pass.
            let Some(zm) = &chunk.zone_map else {
                return false;
            };
            // e.g. price > 20 AND price < 300: skip if max <= 20 or min >= 300
            let lower_bound_cannot_match = match &range.low {
                Bound::Included(low) => {
                    // zm.max < low
                    matches!(zm.max.compare(low), Some(Ordering::Less))
                }
                Bound::Excluded(low) => {
                    // zm.max <= low
                    matches!(zm.max.compare(low), Some(Ordering::Less | Ordering::Equal))
                }
                Bound::Unbounded => false, // no limit can't rule anything out
            };

            if lower_bound_cannot_match {
                return false;
            }

            let upper_bound_cannot_match = match &range.high {
                Bound::Included(high) => {
                    // zm.min > high
                    matches!(zm.min.compare(high), Some(Ordering::Greater))
                }
                Bound::Excluded(high) => {
                    // zm.min >= high
                    matches!(
                        zm.min.compare(high),
                        Some(Ordering::Greater | Ordering::Equal)
                    )
                }
                Bound::Unbounded => false,
            };
            if upper_bound_cannot_match {
                return false;
            }
        }

        // If we made it through all ranges without returning false, it may match
        true
    }
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

        // FOR, 2 bits per value: 3 rows = 6 bits = 1 byte. A 4th row still fits in that
        // byte's padding, so ask for 5 (10 bits, 2 bytes): must fail, not read past the end.
        let mut bytes: &[u8] = &buffer;
        assert!(VectorData::decode(&mut bytes, DataType::Int64, 5).is_err());
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
        let mut buffer = Vec::new();
        VectorData::Varchar(StringVector::from_strs(values)).encode(&mut buffer);
        buffer
    }

    #[test]
    fn decode_vectordata_varchar() {
        // "" is how a NULL slot looks; "café" has 4 chars but 5 bytes
        let values = VectorData::Varchar(StringVector::from_strs(&["AIR", "", "TRUCK", "café"]));
        let mut buffer: Vec<u8> = Vec::new();
        values.encode(&mut buffer);
        assert_eq!(buffer[0], EncodingTag::Plain as u8); // all distinct: plain is smaller
        assert_eq!(buffer.len(), 1 + 5 * 4 + 13); // tag, offsets [0, 3, 3, 8, 13], 13 data bytes

        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 4).unwrap();

        assert_eq!(decoded, values);
        assert!(bytes.is_empty()); // data area consumed too, not just offsets
    }

    // The l_quantity example: min 8, deltas 9 28 0 20 16 24 30 37, 6 bits each.
    const QUANTITIES: [i64; 8] = [17, 36, 8, 28, 24, 32, 38, 45];

    fn int64_round_trip(values: &[i64]) -> Vec<u8> {
        let mut buffer: Vec<u8> = Vec::new();
        VectorData::Int64(values.to_vec()).encode(&mut buffer);
        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Int64, values.len()).unwrap();
        assert_eq!(decoded, VectorData::Int64(values.to_vec()));
        assert!(bytes.is_empty());
        buffer
    }

    #[test]
    fn encode_int64_for_layout() {
        let buffer = int64_round_trip(&QUANTITIES);
        let mut expected = vec![EncodingTag::FrameOfReference as u8];
        expected.extend_from_slice(&8i64.to_le_bytes()); // min
        expected.push(6); // bit_width
        expected.extend_from_slice(&[0x09, 0x07, 0x50, 0x10, 0xE6, 0x95]);
        assert_eq!(buffer, expected);
    }

    #[test]
    fn encode_int64_for_padding() {
        // QUANTITIES without 38: same min and width, 7 rows x 6 bits = 42 bits.
        // The last byte holds 2 bits of the last delta (37) and 6 bits of padding.
        let buffer = int64_round_trip(&[17, 36, 8, 28, 24, 32, 45]);
        assert_eq!(buffer[9], 6);
        assert_eq!(&buffer[10..], &[0x09, 0x07, 0x50, 0x10, 0x56, 0x02]);
    }

    #[test]
    fn encode_int64_for_all_equal() {
        // Spread 0 -> 0 bits: header only.
        let buffer = int64_round_trip(&[5; 100]);
        assert_eq!(buffer.len(), 1 + 8 + 1);
        assert_eq!(buffer[9], 0);
    }

    #[test]
    fn encode_int64_for_negatives() {
        let buffer = int64_round_trip(&[-5, -1, -100, 3, -7]);
        assert_eq!(buffer[0], EncodingTag::FrameOfReference as u8);
        assert_eq!(&buffer[1..9], &(-100i64).to_le_bytes()); // min
        assert_eq!(buffer[9], 7); // spread 103
    }

    #[test]
    fn encode_int64_extremes_pick_plain() {
        // Spread doesn't fit in an i64 (wrapping_sub); 64 bits per value is never smaller than plain.
        let buffer = int64_round_trip(&[i64::MIN, 0, i64::MAX]);
        assert_eq!(buffer[0], EncodingTag::Plain as u8);
        assert_eq!(buffer.len(), 1 + 3 * 8);
    }

    #[test]
    fn encode_int64_wide_for() {
        // 63-bit deltas: the accumulator holds up to 7 + 63 bits, more than a u64.
        // 100 rows so the 1 bit saved per row beats the 9 extra header bytes.
        let values: Vec<i64> = (0..100).map(|i| if i % 2 == 0 { 0 } else { i64::MAX - i }).collect();
        let buffer = int64_round_trip(&values);
        assert_eq!(buffer[0], EncodingTag::FrameOfReference as u8);
        assert_eq!(buffer[9], 63);
    }

    #[test]
    fn decode_int64_for_bad_bit_width() {
        let mut buffer = int64_round_trip(&QUANTITIES);
        buffer[9] = 65;
        let mut bytes: &[u8] = &buffer;
        assert!(VectorData::decode(&mut bytes, DataType::Int64, 8).is_err());
    }

    #[test]
    fn decode_int64_for_truncated() {
        let buffer = int64_round_trip(&QUANTITIES);
        for len in 0..buffer.len() {
            let mut bytes: &[u8] = &buffer[..len];
            assert!(VectorData::decode(&mut bytes, DataType::Int64, 8).is_err(), "len {len}");
        }
    }

    // The 8-row l_shipmode example, repeated 4 times: 32 rows, 4 distinct values, 1-byte codes.
    // Plain 257 bytes, dictionary 73.
    const SHIPMODES: [&str; 8] = [
        "AIR", "TRUCK", "AIR", "MAIL", "TRUCK", "AIR", "RAIL", "MAIL",
    ];
    const SHIPMODE_CODES: [u32; 8] = [0, 1, 0, 2, 1, 0, 3, 2];

    fn shipmodes_x4() -> Vec<&'static str> {
        SHIPMODES.repeat(4)
    }

    #[test]
    fn encode_varchar_picks_plain_when_smaller() {
        // All distinct: plain 37 bytes, dictionary 45 (the values plus a code per row).
        let buffer = varchar_bytes(&["AIR", "TRUCK", "MAIL", "RAIL"]);
        assert_eq!(buffer[0], EncodingTag::Plain as u8);
    }

    #[test]
    fn encode_varchar_two_byte_codes() {
        // 300 distinct values don't fit a 1-byte code: codes take 2 bytes.
        let distinct: Vec<String> = (0..300).map(|i| format!("value_{i:03}")).collect();
        let values: Vec<&str> = (0..3000).map(|i| distinct[i % 300].as_str()).collect();
        let buffer = varchar_bytes(&values);
        assert_eq!(buffer[0], EncodingTag::Dictionary as u8);
        assert_eq!(&buffer[1..5], &300u32.to_le_bytes());
        // tag, num_distinct, 301 offsets, 300 × 9 bytes, 3000 × 2-byte codes
        assert_eq!(buffer.len(), 1 + 4 + 301 * 4 + 2700 + 3000 * 2);
        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 3000).unwrap();
        assert_eq!(
            decoded,
            VectorData::Varchar(StringVector::from_strs(&values))
        );
    }

    #[test]
    fn encode_varchar_dictionary_layout() {
        let buffer = varchar_bytes(&shipmodes_x4());
        let mut expected = vec![EncodingTag::Dictionary as u8];
        expected.extend_from_slice(&4u32.to_le_bytes()); // num_distinct
        for o in [0u32, 3, 8, 12, 16] {
            expected.extend_from_slice(&o.to_le_bytes());
        }
        expected.extend_from_slice(b"AIRTRUCKMAILRAIL");
        for code in SHIPMODE_CODES.repeat(4) {
            expected.push(code as u8); // 4 distinct values: 1-byte codes
        }
        assert_eq!(buffer, expected);
    }

    #[test]
    fn decode_vectordata_varchar_dictionary() {
        let values = shipmodes_x4();
        let buffer = varchar_bytes(&values);
        assert_eq!(buffer[0], EncodingTag::Dictionary as u8);
        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 32).unwrap();
        assert_eq!(
            decoded,
            VectorData::Varchar(StringVector::from_strs(&values))
        );
        assert!(bytes.is_empty()); // codes consumed too
    }

    #[test]
    fn encode_range_varchar_dictionary_middle() {
        // Rows 2..18 only, starting at AIR MAIL TRUCK... Codes restart at 0 for every chunk.
        let values = shipmodes_x4();
        let mut buffer = Vec::new();
        VectorData::Varchar(StringVector::from_strs(&values)).encode_range(2, 16, &mut buffer);
        assert_eq!(buffer[0], EncodingTag::Dictionary as u8);
        assert_eq!(&buffer[1..5], &4u32.to_le_bytes()); // num_distinct
        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 16).unwrap();
        assert_eq!(
            decoded,
            VectorData::Varchar(StringVector::from_strs(&values[2..18]))
        );
    }

    #[test]
    fn decode_vectordata_varchar_dictionary_bad_code() {
        let mut buffer = varchar_bytes(&shipmodes_x4());
        let last_code = buffer.len() - 1;
        buffer[last_code] = 4; // only codes 0..=3 exist
        let mut bytes: &[u8] = &buffer;
        let err = VectorData::decode(&mut bytes, DataType::Varchar, 32).unwrap_err();
        assert!(err.contains("code 4"), "{err}");
    }

    #[test]
    fn decode_vectordata_varchar_dictionary_truncated() {
        let buffer = varchar_bytes(&shipmodes_x4());
        // Cutting anywhere before the end must fail, never panic.
        for len in 0..buffer.len() {
            let mut bytes: &[u8] = &buffer[..len];
            assert!(
                VectorData::decode(&mut bytes, DataType::Varchar, 32).is_err(),
                "len {len}"
            );
        }
    }

    #[test]
    fn decode_vectordata_varchar_unknown_tag() {
        let mut buffer = varchar_bytes(&shipmodes_x4());
        buffer[0] = 7;
        let mut bytes: &[u8] = &buffer;
        let err = VectorData::decode(&mut bytes, DataType::Varchar, 32).unwrap_err();
        assert!(err.contains("unknown encoding tag 7"), "{err}");
    }

    #[test]
    fn decode_vectordata_varchar_reencodes_same_bytes() {
        // decode keeps the on-disk layout as is, so encoding the result gives back the same bytes.
        let buffer = varchar_bytes(&["AIR", "", "TRUCK", "café"]);
        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 4).unwrap();
        let mut reencoded = Vec::new();
        decoded.encode(&mut reencoded);
        assert_eq!(reencoded, buffer);
    }

    #[test]
    fn decode_vectordata_varchar_zero_rows() {
        let buffer = varchar_bytes(&[]); // just the tag and the single offset 0
        let mut bytes: &[u8] = &buffer;
        let decoded = VectorData::decode(&mut bytes, DataType::Varchar, 0).unwrap();
        assert_eq!(decoded, VectorData::Varchar(StringVector::new()));
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
        // tag 0, offsets [0, 3, 8] + "AIRTRUCK"
        let mut first_not_zero = varchar_bytes(&["AIR", "TRUCK"]);
        first_not_zero[1] = 1;
        let mut bytes: &[u8] = &first_not_zero;
        assert!(VectorData::decode(&mut bytes, DataType::Varchar, 2).is_err());

        let mut decreasing = varchar_bytes(&["AIR", "TRUCK"]);
        decreasing[5] = 9; // offsets become [0, 9, 8]
        let mut bytes: &[u8] = &decreasing;
        assert!(VectorData::decode(&mut bytes, DataType::Varchar, 2).is_err());
    }

    #[test]
    fn decode_vectordata_varchar_offset_inside_character() {
        // tag 0, offsets [0, 2, 2] + "é" (2 bytes: 0xC3 0xA9). The data stays valid UTF-8,
        // but moving offset 1 to byte 1 cuts "é" in half: only the boundary check can catch it.
        let mut buffer = varchar_bytes(&["é", ""]);
        buffer[5] = 1; // offsets become [0, 1, 2]
        let mut bytes: &[u8] = &buffer;
        let err = VectorData::decode(&mut bytes, DataType::Varchar, 2).unwrap_err();
        assert!(err.contains("character boundary"), "{err}");
    }

    #[test]
    fn decode_vectordata_varchar_invalid_utf8() {
        let mut buffer = varchar_bytes(&["AIR"]);
        buffer[9] = 0xFF; // first data byte, after the tag and offsets [0, 3]
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
        // 2 bitmap words, then FOR: values 0..=98 -> 7 bits each, 700 bits = 88 bytes.
        assert_eq!(buffer.len(), 16 + 1 + 8 + 1 + 88);

        let mut bytes: &[u8] = &buffer;
        let decoded = Vector::decode(&mut bytes, DataType::Int64, 100, null_count).unwrap();
        assert_eq!(decoded, vector);
        assert!(bytes.is_empty());
    }

    #[test]
    fn vector_round_trip_no_nulls() {
        let vector = Vector {
            data: VectorData::Varchar(StringVector::from_strs(&["AIR", "", "café"])),
            validity: Bitmap::all_valid(3),
        };
        let mut buffer: Vec<u8> = Vec::new();
        let null_count = vector.encode(&mut buffer);
        assert_eq!(null_count, 0);
        assert_eq!(buffer.len(), 1 + 16 + 8); // no bitmap: tag, 4 offsets, then 8 string bytes

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
            data: vector.data.clone().take_range(start, num_rows),
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
        // Values only, FOR: 128..=147 -> 5 bits each, 100 bits = 13 bytes.
        assert_eq!(buffer.len(), 1 + 8 + 1 + 13);
    }

    #[test]
    fn encode_range_varchar_offsets_restart() {
        let names = ["a", "bb", "ccc", "dddd"];
        let mut strings = StringVector::new();
        for i in 0..70 {
            strings.push(names[i % 4]);
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
            data: VectorData::Varchar(StringVector::from_strs(&["TRUCK", "AIR", "", "café"])),
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
        // FOR: group 0 holds 0..=65,535 -> 16 bits; group 1 holds a spread of 4,463 -> 13 bits.
        assert_eq!(chunk.offset, 4 + 1 + 8 + 1 + 65_536 * 2); // right after group 0
        assert_eq!(chunk.chunk_length, 1 + 8 + 1 + (4_464 * 13usize).div_ceil(8) as u32);
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

    // A unique path in the temp dir; tests run in parallel, so each needs its own file.
    fn temp_path(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("olap_{name}.pax"));
        path.to_str().unwrap().to_string()
    }

    // Bytes of a valid PAX file: one Int64 column "a" holding 0..100.
    fn valid_file_bytes(name: &str) -> Vec<u8> {
        let path = temp_path(name);
        write(
            &path,
            &int_schema(&["a"]),
            &[int_column((0..100).collect())],
        )
        .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        bytes
    }

    // Saves `bytes` as a file, opens it, deletes it, and returns what open returned.
    fn open_bytes(name: &str, bytes: &[u8]) -> Result<PaxFile, String> {
        let path = temp_path(name);
        std::fs::write(&path, bytes).unwrap();
        let result = PaxFile::open(&path);
        std::fs::remove_file(&path).unwrap();
        result
    }

    #[test]
    fn open_reads_footer() {
        let path = temp_path("open_reads_footer");
        let schema = int_schema(&["a", "b"]);
        // 70,000 rows: two row groups
        let columns = [
            int_column((0..70_000).collect()),
            int_column((0..70_000).rev().collect()),
        ];
        write(&path, &schema, &columns).unwrap();
        let pax = PaxFile::open(&path);
        std::fs::remove_file(&path).unwrap();

        let footer = pax.unwrap().footer;
        assert_eq!(footer.scheme, schema);
        assert_eq!(footer.row_groups_metadata.len(), 2);
        assert_eq!(footer.row_groups_metadata[0].num_rows, 65_536);
        assert_eq!(footer.row_groups_metadata[1].num_rows, 4_464);
    }

    #[test]
    fn open_missing_file() {
        assert!(PaxFile::open(&temp_path("open_missing_file")).is_err());
    }

    #[test]
    fn open_rejects_too_small() {
        // 11 bytes: one short of the smallest possible file
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&MAGIC);
        assert!(open_bytes("open_rejects_too_small", &bytes).is_err());
    }

    #[test]
    fn open_rejects_bad_start_magic() {
        let mut bytes = valid_file_bytes("open_rejects_bad_start_magic_src");
        bytes[0] = b'X';
        assert!(open_bytes("open_rejects_bad_start_magic", &bytes).is_err());
    }

    #[test]
    fn open_rejects_bad_end_magic() {
        let mut bytes = valid_file_bytes("open_rejects_bad_end_magic_src");
        let last = bytes.len() - 1;
        bytes[last] = b'X';
        assert!(open_bytes("open_rejects_bad_end_magic", &bytes).is_err());
    }

    #[test]
    fn open_rejects_huge_footer_len() {
        let mut bytes = valid_file_bytes("open_rejects_huge_footer_len_src");
        let len_at = bytes.len() - 8;
        bytes[len_at..len_at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        // Must fail cleanly, not underflow `file_len - 8 - footer_len`.
        assert!(open_bytes("open_rejects_huge_footer_len", &bytes).is_err());
    }

    #[test]
    fn open_rejects_wrong_footer_len() {
        let bytes = valid_file_bytes("open_rejects_wrong_footer_len_src");
        let len_at = bytes.len() - 8;
        let footer_len = u32::from_le_bytes(bytes[len_at..len_at + 4].try_into().unwrap());
        // One byte off either way: the footer is read from the wrong place.
        for wrong in [footer_len - 1, footer_len + 1] {
            let mut bad = bytes.clone();
            bad[len_at..len_at + 4].copy_from_slice(&wrong.to_le_bytes());
            assert!(
                open_bytes("open_rejects_wrong_footer_len", &bad).is_err(),
                "footer_len {wrong}"
            );
        }
    }

    // Two columns over 70,000 rows (two row groups): Int64 with a NULL every 1,000 rows,
    // and Varchar with no NULLs, so one chunk has a bitmap and the other doesn't.
    fn mixed_columns() -> (TableSchema, Vec<Vector>) {
        let mut validity = Bitmap::all_valid(70_000);
        for i in (0..70_000).step_by(1_000) {
            validity.set_null(i);
        }
        let ints = Vector {
            data: VectorData::Int64(
                (0..70_000)
                    .map(|i| if i % 1_000 == 0 { 0 } else { i })
                    .collect(),
            ),
            validity,
        };
        let strings = Vector {
            data: VectorData::Varchar((0..70_000).map(|i| format!("s{i}")).collect()),
            validity: Bitmap::all_valid(70_000),
        };
        let schema = TableSchema::new(
            "mixed",
            vec![
                Column::new("n", DataType::Int64),
                Column::new("s", DataType::Varchar),
            ],
        );
        (schema, vec![ints, strings])
    }

    // Writes mixed_columns, opens the file, and deletes it. On Linux an open file
    // stays readable after its name is deleted, so the PaxFile still works.
    fn open_mixed(name: &str) -> (PaxFile, Vec<Vector>) {
        let path = temp_path(name);
        let (schema, columns) = mixed_columns();
        write(&path, &schema, &columns).unwrap();
        let pax = PaxFile::open(&path);
        std::fs::remove_file(&path).unwrap();
        (pax.unwrap(), columns)
    }

    #[test]
    fn read_chunk_round_trip() {
        let (pax, columns) = open_mixed("read_chunk_round_trip");
        // (group, first row, rows): the last group is the short one
        for (group, start, num_rows) in [(0, 0, 65_536), (1, 65_536, 4_464)] {
            for column in 0..2 {
                let chunk = pax.read_chunk(group, column).unwrap();
                assert_eq!(chunk.data.len(), num_rows);
                assert_eq!(
                    chunk,
                    copy_rows(&columns[column], start, num_rows),
                    "group {group} column {column}"
                );
            }
        }
    }

    #[test]
    fn scan_projects_columns() {
        use crate::exec::exec::{Operator, Scan};
        let (pax, columns) = open_mixed("scan_projects_columns");
        // Reversed order: chunk column j holds table column wanted[j], not column j.
        let wanted = [1, 0];
        let mut scan = Scan::new(Box::new(pax), wanted.to_vec(), Vec::new());
        let mut row = 0;
        let mut counts = Vec::new();
        while let Some(chunk) = scan.next().unwrap() {
            assert_eq!(chunk.columns.len(), wanted.len());
            for (j, &column) in wanted.iter().enumerate() {
                assert_eq!(
                    chunk.columns[j],
                    copy_rows(&columns[column], row, chunk.count),
                    "row {row} column {column}"
                );
            }
            row += chunk.count;
            counts.push(chunk.count);
        }
        assert_eq!(row, 70_000);
        // Group 0 gives 32 chunks of 2048; group 1 (4,464 rows) gives 2048, 2048, 368.
        // Chunks never span two groups.
        assert_eq!(counts.len(), 35);
        assert!(counts[..34].iter().all(|&c| c == 2048));
        assert_eq!(counts[34], 368);
    }

    // A range on mixed_columns' Int64 column 0. Its zone maps: group 0 holds 1..=65_535
    // (row 0 is NULL), group 1 holds 65_536..=69_999.
    fn int_range(low: Bound<i64>, high: Bound<i64>) -> ColumnRange {
        ColumnRange {
            column: 0,
            low: low.map(Scalar::I64),
            high: high.map(Scalar::I64),
        }
    }

    // may_match for groups 0 and 1.
    fn groups_kept(pax: &PaxFile, ranges: &[ColumnRange]) -> [bool; 2] {
        [pax.may_match(0, ranges), pax.may_match(1, ranges)]
    }

    #[test]
    fn may_match_skips_groups_outside_range() {
        use Bound::*;
        let (pax, _) = open_mixed("may_match_skips_groups_outside_range");
        assert_eq!(
            groups_kept(&pax, &[int_range(Included(66_000), Unbounded)]),
            [false, true]
        );
        assert_eq!(
            groups_kept(&pax, &[int_range(Unbounded, Excluded(100))]),
            [true, false]
        );
        assert_eq!(
            groups_kept(&pax, &[int_range(Included(70_000), Unbounded)]),
            [false, false]
        );
        assert_eq!(
            groups_kept(&pax, &[int_range(Unbounded, Unbounded)]),
            [true, true]
        );
        assert_eq!(groups_kept(&pax, &[]), [true, true]); // no filter
    }

    #[test]
    fn may_match_edges() {
        use Bound::*;
        let (pax, _) = open_mixed("may_match_edges");
        // Group 0's max is 65_535, group 1's min is 65_536.
        assert_eq!(
            groups_kept(&pax, &[int_range(Included(65_535), Unbounded)]),
            [true, true]
        );
        assert_eq!(
            groups_kept(&pax, &[int_range(Excluded(65_535), Unbounded)]),
            [false, true]
        );
        assert_eq!(
            groups_kept(&pax, &[int_range(Unbounded, Included(65_536))]),
            [true, true]
        );
        assert_eq!(
            groups_kept(&pax, &[int_range(Unbounded, Excluded(65_536))]),
            [true, false]
        );
    }

    #[test]
    fn may_match_ands_ranges() {
        use Bound::*;
        let (pax, _) = open_mixed("may_match_ands_ranges");
        // Each range alone keeps one group, a different one; ANDed, neither group fits both.
        let only_group_0 = int_range(Unbounded, Excluded(65_536));
        let only_group_1 = int_range(Included(65_536), Unbounded);
        assert_eq!(groups_kept(&pax, &[only_group_0.clone()]), [true, false]);
        assert_eq!(groups_kept(&pax, &[only_group_1.clone()]), [false, true]);
        assert_eq!(
            groups_kept(&pax, &[only_group_0, only_group_1]),
            [false, false]
        );
    }

    #[test]
    fn may_match_keeps_when_unsure() {
        let (pax, _) = open_mixed("may_match_keeps_when_unsure");
        // A Date bound on an Int64 column: compare says None, so nothing may be skipped.
        let wrong_type = ColumnRange {
            column: 0,
            low: Bound::Included(Scalar::Date(1_000_000)),
            high: Bound::Unbounded,
        };
        assert_eq!(groups_kept(&pax, &[wrong_type]), [true, true]);
        let unknown_column = ColumnRange {
            column: 99,
            ..int_range(Bound::Included(1_000_000), Bound::Unbounded)
        };
        assert_eq!(groups_kept(&pax, &[unknown_column]), [true, true]);
    }

    #[test]
    fn may_match_skips_all_null_chunk() {
        let path = temp_path("may_match_skips_all_null_chunk");
        let schema = TableSchema::new("t", vec![Column::new("a", DataType::Int64)]);
        let mut validity = Bitmap::all_valid(100);
        for i in 0..100 {
            validity.set_null(i);
        }
        let column = Vector {
            data: VectorData::Int64(vec![0; 100]),
            validity,
        };
        write(&path, &schema, &[column]).unwrap();
        let pax = PaxFile::open(&path);
        std::fs::remove_file(&path).unwrap();
        // No zone map at all; even an unbounded range can't match NULLs.
        let range = int_range(Bound::Unbounded, Bound::Unbounded);
        assert!(!pax.unwrap().may_match(0, &[range]));
    }

    // Scans mixed_columns' column 0 with `filters`; checks every chunk against the original
    // rows, starting at row `first_row`. Returns (rows scanned, groups skipped).
    fn scan_filtered(name: &str, filters: Vec<ColumnRange>, first_row: usize) -> (usize, usize) {
        use crate::exec::exec::{Operator, Scan};
        let (pax, columns) = open_mixed(name);
        let mut scan = Scan::new(Box::new(pax), vec![0], filters);
        let mut row = first_row;
        while let Some(chunk) = scan.next().unwrap() {
            assert_eq!(
                chunk.columns[0],
                copy_rows(&columns[0], row, chunk.count),
                "row {row}"
            );
            row += chunk.count;
        }
        (row - first_row, scan.skipped_groups())
    }

    #[test]
    fn scan_skips_second_group() {
        // A skipped group after a read one must not hand out the read group's rows again.
        let filters = vec![int_range(Bound::Unbounded, Bound::Excluded(65_536))];
        assert_eq!(
            scan_filtered("scan_skips_second_group", filters, 0),
            (65_536, 1)
        );
    }

    #[test]
    fn scan_skips_first_group() {
        let filters = vec![int_range(Bound::Included(65_536), Bound::Unbounded)];
        assert_eq!(
            scan_filtered("scan_skips_first_group", filters, 65_536),
            (4_464, 1)
        );
    }

    #[test]
    fn scan_skips_every_group() {
        let filters = vec![int_range(Bound::Included(70_000), Bound::Unbounded)];
        assert_eq!(scan_filtered("scan_skips_every_group", filters, 0), (0, 2));
    }

    #[test]
    fn scan_without_filters_reads_every_group() {
        assert_eq!(
            scan_filtered("scan_without_filters", Vec::new(), 0),
            (70_000, 0)
        );
    }

    #[test]
    fn read_chunk_out_of_range() {
        let (pax, _) = open_mixed("read_chunk_out_of_range");
        assert!(pax.read_chunk(2, 0).is_err()); // only groups 0 and 1
        assert!(pax.read_chunk(0, 2).is_err()); // only columns 0 and 1
    }

    #[test]
    fn read_chunk_corrupt_varchar() {
        let path = temp_path("read_chunk_corrupt_varchar_src");
        let (schema, columns) = mixed_columns();
        write(&path, &schema, &columns).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let pax = PaxFile::open(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        // No NULLs in "s", so its chunk starts straight with the offsets; the first must be 0.
        let offset = pax.footer.row_groups_metadata[0].chunks_metadata[1].offset as usize;
        bytes[offset] = 1;
        let bad = open_bytes("read_chunk_corrupt_varchar", &bytes).unwrap();

        let err = bad.read_chunk(0, 1).err().unwrap();
        assert!(err.contains("row group 0 column 1"), "{err}");
        assert!(bad.read_chunk(0, 0).is_ok()); // other chunks are untouched
    }
}
