use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DataType {
    Int64 = 1,
    Float64 = 2,
    Date = 3, // stored as i32 days since 1970-01-01
    Varchar = 4,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<Column>, // same order as the CSV header
}

#[derive(Debug)]
pub struct Catalog {
    tables: HashMap<String, TableSchema>,
}

impl Catalog {
    /// Hardcoded TPC-H schemas. Add more tables as queries need them.
    pub fn tpch() -> Self {
        use DataType::*;

        let mut catalog = Catalog {
            tables: HashMap::new(),
        };

        catalog.add_table(
            "lineitem",
            &[
                ("l_orderkey", Int64),
                ("l_partkey", Int64),
                ("l_suppkey", Int64),
                ("l_linenumber", Int64),
                ("l_quantity", Float64),
                ("l_extendedprice", Float64),
                ("l_discount", Float64),
                ("l_tax", Float64),
                ("l_returnflag", Varchar),
                ("l_linestatus", Varchar),
                ("l_shipdate", Date),
                ("l_commitdate", Date),
                ("l_receiptdate", Date),
                ("l_shipinstruct", Varchar),
                ("l_shipmode", Varchar),
                ("l_comment", Varchar),
            ],
        );

        catalog
    }

    fn add_table(&mut self, name: &str, columns: &[(&str, DataType)]) {
        let columns = columns
            .iter()
            .map(|&(col_name, ty)| Column {
                name: col_name.to_string(),
                ty,
            })
            .collect();

        self.tables.insert(
            name.to_string(),
            TableSchema {
                name: name.to_string(),
                columns,
            },
        );
    }

    pub fn get(&self, name: &str) -> Option<&TableSchema> {
        self.tables.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lineitem_schema() {
        let catalog = Catalog::tpch();
        let lineitem = catalog.get("lineitem").unwrap();
        assert_eq!(lineitem.columns.len(), 16);
        assert_eq!(lineitem.columns[10].name, "l_shipdate");
        assert_eq!(lineitem.columns[10].ty, DataType::Date);
        assert!(catalog.get("no_such_table").is_none());
    }
}
