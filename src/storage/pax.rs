use std::str::from_utf8;

use crate::catalog::{Column, DataType, TableSchema};

pub enum Scalar {
    I64(i64),
    F64(f64),
    I32(i32),
    String(String),
}

struct ZoneMap {
    min: Scalar,
    max: Scalar,
}

struct ChunkMetadata {
    chunk_length: u32,
    offset: u64,
    null_count: u32,
    zone_map: ZoneMap,
}

struct RowGroupMeta {
    row_group_id: u32,
    num_rows: u32,
    total_byte_size: u64,
    chunks_metadata: Vec<ChunkMetadata>,
}

// will be loaded on first access
pub struct PaxFooter {
    row_groups_metadata: Vec<RowGroupMeta>,
    scheme: TableSchema,
}

impl Column {
    // A helper to make creating columns less verbose
    pub fn new(name: &str, ty: DataType) -> Self {
        Self {
            name: name.to_string(),
            ty,
        }
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
}

// we want to pull the metadata, then stream chunks of Vectors at certain offsets
fn read() {}
