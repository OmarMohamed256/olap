use crate::{
    catalog::{Column, DataType, TableSchema},
    exec::exec::{Bitmap, VectorData},
};
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
        match self {
            VectorData::Int64(v) => {
                buffer.reserve(v.len() * 8);
                for &val in v {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            VectorData::Float64(v) => {
                buffer.reserve(v.len() * 8);
                for &val in v {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            VectorData::Date(v) => {
                buffer.reserve(v.len() * 4);
                for &val in v {
                    buffer.extend_from_slice(&val.to_le_bytes());
                }
            }
            VectorData::Varchar(v) => {
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
        buffer.reserve(self.words.len() * 8);
        for &word in &self.words {
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
// we want to pull the metadata, then stream chunks of Vectors at certain offsets
fn read() {}

fn take_bytes<'a>(buffer: &mut &'a [u8], len: usize, err_msg: &str) -> Result<&'a [u8], String> {
    if buffer.len() < len {
        return Err(err_msg.to_string());
    }
    let (chunk, rest) = buffer.split_at(len);
    *buffer = rest;
    Ok(chunk)
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
}
