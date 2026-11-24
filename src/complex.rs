//! Port of the `com.healthmarketscience.jackcess.complex` package and the
//! `impl` classes which support it (`ComplexColumnSupport`,
//! `ComplexColumnCreator`, `ComplexColumnDesc`, `ComplexColumnInfoImpl`,
//! `MultiValueColumnInfoImpl`, `AttachmentColumnInfoImpl`,
//! `VersionHistoryColumnInfoImpl`, `ComplexValueForeignKeyImpl` and
//! `com.healthmarketscience.jackcess.complex.Version`).
//!
//! A complex column (multi-value, attachment or version history) holds only a
//! foreign key id in the main table.  The values live in a per column "flat"
//! table whose shape is copied from a database level "type" table (a version
//! history column's type table is created per column instead).  A row of
//! `MSysComplexColumns` ties the column, its type table and its flat table
//! together.

use crate::column::{Column, ColumnSpec, COMPLEX_FK_EXT_FLAG_MASK};
use crate::database::Database;
use crate::datatype::DataType;
use crate::datenums::LocalDateTime;
use crate::error::{Error, Result};
use crate::table::{RowId, Table};
use crate::value::Value;

/// the catalog flags of a table which holds a complex column (`MSysObjects`
/// flags, without which access never looks for the values)
pub const CATALOG_FLAGS_HAS_COMPLEX: i32 = 0x0004_0000;
/// the catalog flags of a flat table (low bits mark it as complex support,
/// `0x80000000` hides it from the navigation pane)
pub const CATALOG_FLAGS_FLAT_TABLE: i32 = 0x800A_0000u32 as i32;
/// the catalog flags of a version history type table
pub const CATALOG_FLAGS_TYPE_TABLE: i32 = 0x8003_0000u32 as i32;

pub const COMPLEX_COLUMNS_TABLE: &str = "MSysComplexColumns";
pub const COL_COMPLEX_ID: &str = "ComplexID";
pub const COL_TABLE_ID: &str = "ConceptualTableID";
pub const COL_COMPLEX_TYPE_OBJECT_ID: &str = "ComplexTypeObjectID";
pub const COL_FLAT_TABLE_ID: &str = "FlatTableID";
pub const COL_COLUMN_NAME: &str = "ColumnName";

pub const TYPE_TABLE_PREFIX: &str = "MSysComplexType_";
pub const ATTACHMENT_TYPE_TABLE: &str = "MSysComplexType_Attachment";
pub const VERSION_HISTORY_TYPE_PREFIX: &str = "MSysComplexTypeVH_";
pub const FLAT_TABLE_PREFIX: &str = "f_";
pub const FLAT_PK_INDEX_NAME: &str = "MSysComplexPKIndex";

pub const VERSION_HISTORY_COL_NAME: &str = "VersionHistory_F5F8918F-0A3F-4DA9-AE71-184EE5012880";
/// the date column of every version history type table
/// (`ComplexColumnCreator.MODIFIED_COL_NAME`)
pub const MODIFIED_COL_NAME: &str = "Modified_F9B5E312-4155-4c59-9AAE-391C1B295827";

const FILE_NAME_COL_NAME: &str = "FileName";
const FILE_TYPE_COL_NAME: &str = "FileType";

/// `ComplexDataType`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComplexDataType {
    Attachment,
    MultiValue,
    VersionHistory,
    Unsupported,
}

/// `ComplexColumnDesc`: what a `ColumnBuilder` declares about a complex column.
#[derive(Clone, Debug, PartialEq)]
pub struct ComplexColumnDesc {
    pub kind: ComplexDataType,
    pub value_type: Option<DataType>,
    pub memo_column_name: Option<String>,
    pub row_source_type: Option<String>,
    pub row_source: Option<String>,
    pub column_count: i32,
}

impl ComplexColumnDesc {
    pub fn multi_value(value_type: DataType) -> ComplexColumnDesc {
        ComplexColumnDesc {
            kind: ComplexDataType::MultiValue,
            value_type: Some(value_type),
            memo_column_name: None,
            row_source_type: None,
            row_source: None,
            column_count: 1,
        }
    }

    pub fn attachment() -> ComplexColumnDesc {
        ComplexColumnDesc {
            kind: ComplexDataType::Attachment,
            value_type: None,
            memo_column_name: None,
            row_source_type: None,
            row_source: None,
            column_count: 1,
        }
    }

    pub fn version_history(memo_column_name: impl Into<String>) -> ComplexColumnDesc {
        ComplexColumnDesc {
            kind: ComplexDataType::VersionHistory,
            value_type: None,
            memo_column_name: Some(memo_column_name.into()),
            row_source_type: None,
            row_source: None,
            column_count: 1,
        }
    }

    pub fn set_row_source(
        &mut self,
        row_source_type: impl Into<String>,
        row_source: impl Into<String>,
        column_count: i32,
    ) {
        self.row_source_type = Some(row_source_type.into());
        self.row_source = Some(row_source.into());
        self.column_count = column_count;
    }

    /// `ComplexColumnDesc.copy`.
    pub fn copy(&self) -> ComplexColumnDesc {
        self.clone()
    }

    /// `ComplexColumnDesc.validate`.
    pub fn validate(&self, col_name: &str) -> Result<()> {
        if self.kind != ComplexDataType::MultiValue {
            return Ok(());
        }
        let value_type = self.value_type.unwrap_or(DataType::UnsupportedFixedLen);
        if !multi_value_value_types().contains(&value_type) {
            return Err(Error::IllegalArgument(format!(
                "A multi-value column holds values of type {:?}, not {} (Column={})",
                multi_value_value_types(),
                value_type.get_name(),
                col_name
            )));
        }
        match &self.row_source {
            Some(rs) if !rs.is_empty() => Ok(()),
            _ => Err(Error::IllegalArgument(format!(
                "A multi-value column needs a row source, from either setValueList \
                 or setRowSource (Column={})",
                col_name
            ))),
        }
    }
}

/// `ComplexColumnCreator.getTypeTableName`: the shipped type table of a
/// multi-value value type.
pub fn multi_value_type_table_name(value_type: DataType) -> Option<&'static str> {
    Some(match value_type {
        DataType::Byte => "MSysComplexType_UnsignedByte",
        DataType::Int => "MSysComplexType_Short",
        DataType::Long => "MSysComplexType_Long",
        DataType::Float => "MSysComplexType_IEEESingle",
        DataType::Double => "MSysComplexType_IEEEDouble",
        DataType::Guid => "MSysComplexType_GUID",
        DataType::Numeric => "MSysComplexType_Decimal",
        DataType::Text => "MSysComplexType_Text",
        _ => return None,
    })
}

fn multi_value_value_types() -> &'static [DataType] {
    &[
        DataType::Byte,
        DataType::Int,
        DataType::Long,
        DataType::Float,
        DataType::Double,
        DataType::Guid,
        DataType::Numeric,
        DataType::Text,
        DataType::BigInt,
    ]
}

/// `ComplexColumnSupport.isMultiValueColumn`.
pub fn is_multi_value_column(table: &Table) -> bool {
    table.columns.len() == 1 && multi_value_value_types().contains(&table.columns[0].data_type)
}

/// `ComplexColumnSupport.isAttachmentColumn`.
pub fn is_attachment_column(table: &Table) -> bool {
    let mut num_memo = 0;
    let mut num_text = 0;
    let mut num_date = 0;
    let mut num_ole = 0;
    let mut num_long = 0;
    for col in &table.columns {
        match col.data_type {
            DataType::Text => num_text += 1,
            DataType::Long => num_long += 1,
            DataType::ShortDateTime => num_date += 1,
            DataType::Ole => num_ole += 1,
            DataType::Memo => num_memo += 1,
            _ => {}
        }
    }
    table.columns.len() >= 6
        && num_memo >= 1
        && num_text >= 2
        && num_ole >= 1
        && num_date >= 1
        && num_long >= 1
}

/// `ComplexColumnSupport.isVersionHistoryColumn`.
pub fn is_version_history_column(table: &Table) -> bool {
    let num_memo = table
        .columns
        .iter()
        .filter(|c| c.data_type == DataType::Memo)
        .count();
    let num_date = table
        .columns
        .iter()
        .filter(|c| c.data_type == DataType::ShortDateTime)
        .count();
    table.columns.len() >= 2 && num_memo >= 1 && num_date >= 1
}

/// The columns of the attachment type table which hold the parts of an
/// attachment.
#[derive(Clone, Debug)]
pub struct AttachmentColumns {
    pub url: String,
    pub name: String,
    pub file_type: String,
    pub data: String,
    pub timestamp: String,
    pub flags: String,
}

/// `ComplexColumnInfoImpl` plus its subclasses: everything needed to read and
/// write the values of one complex column.
#[derive(Clone, Debug)]
pub struct ComplexColumnInfo {
    /// name of the column in the main table
    pub column_name: String,
    /// the id of the `MSysComplexColumns` row
    pub complex_type_id: i32,
    pub kind: ComplexDataType,
    /// table definition page of the flat table
    pub flat_table_page: i32,
    /// table definition page of the shape ("type") table
    pub type_table_page: i32,
    /// the auto-number column of the flat table (the id of a value)
    pub pk_column: String,
    /// the LONG column of the flat table which holds the main row's id
    pub fk_column: String,
    /// the value columns (the ones copied from the type table), in flat table
    /// column order
    pub type_columns: Vec<String>,
    /// the value column of a multi-value column, or the memo column whose
    /// values a version history column tracks
    pub value_column: Option<String>,
    /// the `Modified_...` date column of a version history flat table
    pub modified_column: Option<String>,
    /// the parts of an attachment
    pub attachment: Option<AttachmentColumns>,
}

impl ComplexColumnInfo {
    /// `ComplexColumnInfoImpl.countValues`.
    pub fn count_values(&self, db: &mut Database, complex_value_fk: i32) -> Result<usize> {
        Ok(self.raw_values(db, complex_value_fk)?.len())
    }

    /// `ComplexColumnInfoImpl.getRawValues`: the rows of the flat table which
    /// belong to the given main table row id.
    pub fn raw_values(
        &self,
        db: &mut Database,
        complex_value_fk: i32,
    ) -> Result<Vec<(RowId, Vec<Value>)>> {
        let flat_page = self.flat_table_page;
        db.load_table_by_page(flat_page)?;
        let rows = {
            let table = db.tables.get(&flat_page).unwrap();
            table.read_all_rows_with_ids(&mut db.channel)?
        };
        let table = db.tables.get(&flat_page).unwrap();
        let fk_idx = table
            .column_index(&self.fk_column)
            .ok_or_else(|| Error::IllegalState("flat table has no fk column".into()))?;
        let mut out = Vec::new();
        for (row_id, values) in rows {
            let values = match values {
                Some(v) => v,
                None => continue,
            };
            if values
                .get(fk_idx)
                .and_then(|v| v.as_i64())
                .map(|v| v as i32)
                == Some(complex_value_fk)
            {
                out.push((row_id, values));
            }
        }
        // the values are read through the foreign key index, so equal keys are
        // visited in row order
        out.sort_by_key(|(row_id, _)| *row_id);
        Ok(out)
    }

    fn value_id(&self, db: &mut Database, row: &[Value]) -> Result<i32> {
        let table = db.tables.get(&self.flat_table_page).unwrap();
        let pk_idx = table
            .column_index(&self.pk_column)
            .ok_or_else(|| Error::IllegalState("flat table has no pk column".into()))?;
        row.get(pk_idx)
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .ok_or_else(|| Error::IllegalState("flat row has no id".into()))
    }

    /// The values of the column for one main table row (`getValues`).
    pub fn values(&self, db: &mut Database, complex_value_fk: i32) -> Result<Vec<ComplexValue>> {
        let raw = self.raw_values(db, complex_value_fk)?;
        let mut out = Vec::with_capacity(raw.len());
        for (row_id, row) in raw {
            let id = self.value_id(db, &row)?;
            let mut value = ComplexValue {
                id,
                row_id,
                complex_value_fk,
                value: Value::Null,
                attachment: None,
            };
            let table = db.tables.get(&self.flat_table_page).unwrap();
            if let Some(att) = &self.attachment {
                let get = |name: &str| -> Value {
                    table
                        .column_index(name)
                        .and_then(|i| row.get(i).cloned())
                        .unwrap_or(Value::Null)
                };
                let text = |name: &str| -> Option<String> {
                    match get(name) {
                        Value::Text(s) => Some(s),
                        _ => None,
                    }
                };
                let encoded_data = match get(&att.data) {
                    Value::Bytes(b) => Some(b),
                    _ => None,
                };
                let data = encoded_data
                    .as_ref()
                    .and_then(|encoded| decode_attachment_data(encoded).ok());
                value.attachment = Some(AttachmentValue {
                    id,
                    row_id,
                    url: text(&att.url),
                    name: text(&att.name),
                    file_type: text(&att.file_type),
                    data,
                    encoded_data,
                    timestamp: match get(&att.timestamp) {
                        Value::DateTime(ldt) => Some(ldt),
                        _ => None,
                    },
                    flags: get(&att.flags).as_i64().map(|v| v as i32),
                });
            } else if let Some(col) = &self.value_column {
                value.value = table
                    .column_index(col)
                    .and_then(|i| row.get(i).cloned())
                    .unwrap_or(Value::Null);
            }
            out.push(value);
        }
        Ok(out)
    }
}

/// One value of a complex column: the id and row of its flat table row, the
/// main row's foreign key, the multi-value value or the attachment parts.
#[derive(Clone, Debug)]
pub struct ComplexValue {
    pub id: i32,
    pub row_id: RowId,
    pub complex_value_fk: i32,
    pub value: Value,
    pub attachment: Option<AttachmentValue>,
}

#[derive(Clone, Debug)]
pub struct AttachmentValue {
    pub id: i32,
    pub row_id: RowId,
    pub url: Option<String>,
    pub name: Option<String>,
    pub file_type: Option<String>,
    /// the decoded file content (the `Attachment.getFileData` value)
    pub data: Option<Vec<u8>>,
    /// the raw stored wrapper (`Attachment.getEncodedFileData`), for a value
    /// read from the database whose type is one access compresses
    pub encoded_data: Option<Vec<u8>>,
    pub timestamp: Option<LocalDateTime>,
    pub flags: Option<i32>,
}

/// One version of an append-only memo column (`com.healthmarketscience.
/// jackcess.complex.Version`): the memo value, the date it was written and the
/// id/row of its flat table row.
#[derive(Clone, Debug, PartialEq)]
pub struct Version {
    pub id: i32,
    pub row_id: RowId,
    pub complex_value_fk: i32,
    pub value: Option<String>,
    pub modified_date: Option<LocalDateTime>,
}

/// Loads the complex column info of a column of the table on the given
/// definition page (`ComplexColumnSupport.create`).
pub fn load_complex_info(
    db: &mut Database,
    table_page: i32,
    column: &Column,
) -> Result<ComplexColumnInfo> {
    let complex_type_id = column.complex_id;
    let cc_page = match db.table_lookup_entry(COMPLEX_COLUMNS_TABLE) {
        Some((p, _)) => p,
        None => {
            return Err(Error::Jackcess(format!(
                "The database has no {} table (Column={})",
                COMPLEX_COLUMNS_TABLE, column.name
            )))
        }
    };
    db.load_table_by_page(cc_page)?;
    let rows = {
        let table = db.tables.get(&cc_page).unwrap();
        table.read_all_rows(&mut db.channel)?
    };
    let (id_idx, table_idx, type_idx, flat_idx, name_idx) = {
        let table = db.tables.get(&cc_page).unwrap();
        (
            table.column_index(COL_COMPLEX_ID),
            table.column_index(COL_TABLE_ID),
            table.column_index(COL_COMPLEX_TYPE_OBJECT_ID),
            table.column_index(COL_FLAT_TABLE_ID),
            table.column_index(COL_COLUMN_NAME),
        )
    };
    let mut cc_row: Option<Vec<Value>> = None;
    for row in rows.into_iter().flatten() {
        if id_idx.and_then(|i| row.get(i)).and_then(|v| v.as_i64()) == Some(complex_type_id as i64)
        {
            cc_row = Some(row);
            break;
        }
    }
    let cc_row = cc_row.ok_or_else(|| {
        Error::Jackcess(format!(
            "Could not find complex column info for complex column with id {} (Column={})",
            complex_type_id, column.name
        ))
    })?;
    let get_int = |idx: Option<usize>| -> Result<i32> {
        idx.and_then(|i| cc_row.get(i))
            .and_then(|v| v.as_i64())
            .map(|v| v as i32)
            .ok_or_else(|| Error::Jackcess("MSysComplexColumns row is incomplete".into()))
    };
    let table_id = get_int(table_idx)?;
    if table_id != table_page {
        return Err(Error::Jackcess(format!(
            "Found complex column for table {} but expected table {} (Column={})",
            table_id, table_page, column.name
        )));
    }
    if let Some(col_name) = name_idx
        .and_then(|i| cc_row.get(i))
        .and_then(|v| v.as_text())
        .map(|s| s.to_string())
    {
        if !col_name.eq_ignore_ascii_case(&column.name) {
            return Err(Error::Jackcess(format!(
                "Found complex column info for column {} but expected column {}",
                col_name, column.name
            )));
        }
    }
    let flat_table_page = get_int(flat_idx)?;
    let type_table_page = get_int(type_idx)?;

    db.load_table_by_page(type_table_page)?;
    db.load_table_by_page(flat_table_page)?;
    let type_name = db
        .tables
        .get(&type_table_page)
        .map(|t| t.name.clone())
        .unwrap_or_default();
    let upper = type_name.to_uppercase();

    let kind = if type_name.eq_ignore_ascii_case(ATTACHMENT_TYPE_TABLE) {
        ComplexDataType::Attachment
    } else if upper.starts_with(&VERSION_HISTORY_TYPE_PREFIX.to_uppercase()) {
        ComplexDataType::VersionHistory
    } else if upper.starts_with(&TYPE_TABLE_PREFIX.to_uppercase()) {
        let t = db.tables.get(&type_table_page).unwrap();
        if is_multi_value_column(t) {
            ComplexDataType::MultiValue
        } else {
            ComplexDataType::Unsupported
        }
    } else if (column.ext_flags & crate::column::VERSION_HISTORY_EXT_FLAG_MASK) != 0
        && db
            .tables
            .get(&type_table_page)
            .map(is_version_history_column)
            .unwrap_or(false)
    {
        // the ext flag marks a version history column even when its type table
        // has an unexpected name
        ComplexDataType::VersionHistory
    } else {
        let t = db.tables.get(&type_table_page).unwrap();
        if is_multi_value_column(t) {
            ComplexDataType::MultiValue
        } else if is_attachment_column(t) {
            ComplexDataType::Attachment
        } else if is_version_history_column(t) {
            ComplexDataType::VersionHistory
        } else {
            ComplexDataType::Unsupported
        }
    };

    // separate the flat table columns into the value columns (named by the
    // type table) and the other ones
    let type_col_names: Vec<String> = db
        .tables
        .get(&type_table_page)
        .map(|t| t.columns.iter().map(|c| c.name.clone()).collect())
        .unwrap_or_default();
    let mut pk_column = None;
    let mut fk_column = None;
    let mut other_cols: Vec<Column> = Vec::new();
    let mut type_columns: Vec<String> = Vec::new();
    {
        let table = db.tables.get(&flat_table_page).unwrap();
        for col in &table.columns {
            if type_col_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&col.name))
            {
                type_columns.push(col.name.clone());
            } else {
                other_cols.push(col.clone());
            }
        }
    }
    for col in &other_cols {
        if col.complex_value_fk {
            fk_column = Some(col.name.clone());
        } else if col.auto_number {
            pk_column = Some(col.name.clone());
        }
    }
    if fk_column.is_none() {
        for col in &other_cols {
            if !col.auto_number && col.data_type == DataType::Long {
                fk_column = Some(col.name.clone());
            }
        }
    }
    let pk_column = pk_column.ok_or_else(|| {
        Error::Jackcess(format!(
            "Could not find expected columns in flat table for complex column with id {} (Column={})",
            complex_type_id, column.name
        ))
    })?;
    let fk_column = fk_column.ok_or_else(|| {
        Error::Jackcess(format!(
            "Could not find expected columns in flat table for complex column with id {} (Column={})",
            complex_type_id, column.name
        ))
    })?;

    let value_column = if kind == ComplexDataType::MultiValue {
        type_columns.first().cloned()
    } else if kind == ComplexDataType::VersionHistory {
        // `VersionHistoryColumnInfoImpl.getValueColumn`: the memo column of the
        // type table holds one version of the memo column's value
        let flat_columns = db
            .tables
            .get(&flat_table_page)
            .map(|t| t.columns.clone())
            .unwrap_or_default();
        type_columns
            .iter()
            .find(|name| {
                flat_columns
                    .iter()
                    .any(|c| c.name.eq_ignore_ascii_case(name) && c.data_type == DataType::Memo)
            })
            .cloned()
    } else {
        None
    };

    let modified_column = if kind == ComplexDataType::VersionHistory {
        // `VersionHistoryColumnInfoImpl.getModifiedDateColumn`
        let flat_columns = db
            .tables
            .get(&flat_table_page)
            .map(|t| t.columns.clone())
            .unwrap_or_default();
        type_columns
            .iter()
            .find(|name| {
                flat_columns.iter().any(|c| {
                    c.name.eq_ignore_ascii_case(name) && c.data_type == DataType::ShortDateTime
                })
            })
            .cloned()
    } else {
        None
    };

    let attachment = if kind == ComplexDataType::Attachment {
        let mut url = None;
        let mut name = None;
        let mut file_type = None;
        let mut data = None;
        let mut timestamp = None;
        let mut flags = None;
        for col in db
            .tables
            .get(&flat_table_page)
            .map(|t| t.columns.clone())
            .unwrap_or_default()
        {
            if !type_columns
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&col.name))
            {
                continue;
            }
            match col.data_type {
                DataType::Text => {
                    if col.name.eq_ignore_ascii_case(FILE_NAME_COL_NAME) {
                        name = Some(col.name.clone());
                    } else if col.name.eq_ignore_ascii_case(FILE_TYPE_COL_NAME) {
                        file_type = Some(col.name.clone());
                    } else if name.is_none() {
                        name = Some(col.name.clone());
                    } else if file_type.is_none() {
                        file_type = Some(col.name.clone());
                    }
                }
                DataType::Long => flags = Some(col.name.clone()),
                DataType::ShortDateTime => timestamp = Some(col.name.clone()),
                DataType::Ole => data = Some(col.name.clone()),
                DataType::Memo => url = Some(col.name.clone()),
                _ => {}
            }
        }
        Some(AttachmentColumns {
            url: url.unwrap_or_else(|| "FileURL".to_string()),
            name: name.unwrap_or_else(|| FILE_NAME_COL_NAME.to_string()),
            file_type: file_type.unwrap_or_else(|| FILE_TYPE_COL_NAME.to_string()),
            data: data.unwrap_or_else(|| "FileData".to_string()),
            timestamp: timestamp.unwrap_or_else(|| "FileTimeStamp".to_string()),
            flags: flags.unwrap_or_else(|| "FileFlags".to_string()),
        })
    } else {
        None
    };

    Ok(ComplexColumnInfo {
        column_name: column.name.clone(),
        complex_type_id,
        kind,
        flat_table_page,
        type_table_page,
        pk_column,
        fk_column,
        type_columns,
        value_column,
        modified_column,
        attachment,
    })
}

// ---------------------------------------------------------------------------
// attachment data encoding
// ---------------------------------------------------------------------------

const DATA_TYPE_RAW: i32 = 0;
const DATA_TYPE_COMPRESSED: i32 = 1;
const CONTENT_HEADER_FLAG: i32 = 1;
const WRAPPER_HEADER_SIZE: usize = 8;
const CONTENT_HEADER_SIZE: usize = 12;

/// The file types access stores raw (`AttachmentColumnInfoImpl.
/// COMPRESSED_FORMATS`).  Everything else is deflate compressed with zlib
/// level 3 (see [`crate::deflate`]).
pub fn is_raw_attachment_type(file_type: &str) -> bool {
    matches!(
        file_type.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "gif" | "png" | "zip" | "cab" | "docx" | "xlsx" | "xlsb" | "pptx"
    )
}

/// `AttachmentImpl.encodeData`: the stored wrapper (the two header ints, the
/// content header with the file extension and the file data, deflated for the
/// types access does not store raw).
pub fn encode_attachment_data(file_type: &str, data: &[u8]) -> Result<Vec<u8>> {
    // the file extension is lower cased for both the format check and the
    // content header (`AttachmentImpl.encodeData`)
    let type_lower = file_type.to_lowercase();
    let should_compress = !is_raw_attachment_type(&type_lower);

    let mut encoded_type = type_lower;
    encoded_type.push('\0');
    let type_bytes =
        crate::column::encode_uncompressed_text(&encoded_type, crate::format::Charset::Utf16Le);
    let header_len = type_bytes.len() + CONTENT_HEADER_SIZE;

    // the content header: header length, CONTENT_HEADER_FLAG, the character
    // count of the extension string (including its null terminator) and the
    // UTF-16LE extension
    let mut content = Vec::with_capacity(header_len + data.len());
    content.extend_from_slice(&(header_len as i32).to_le_bytes());
    content.extend_from_slice(&CONTENT_HEADER_FLAG.to_le_bytes());
    content.extend_from_slice(&(encoded_type.encode_utf16().count() as i32).to_le_bytes());
    content.extend_from_slice(&type_bytes);
    content.extend_from_slice(data);

    let (type_flag, payload) = if should_compress {
        (
            DATA_TYPE_COMPRESSED,
            crate::deflate::zlib_compress(&content)?,
        )
    } else {
        (DATA_TYPE_RAW, content)
    };

    // the wrapper header: the data type flag and the *uncompressed* length
    let mut out = Vec::with_capacity(WRAPPER_HEADER_SIZE + payload.len());
    out.extend_from_slice(&type_flag.to_le_bytes());
    out.extend_from_slice(&((data.len() + header_len) as i32).to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(out)
}

/// `AttachmentImpl.decodeData`: the file data of an encoded attachment, which
/// access may have deflated.
pub fn decode_attachment_data(encoded: &[u8]) -> Result<Vec<u8>> {
    if encoded.len() < WRAPPER_HEADER_SIZE {
        // nothing we can do
        return Err(Error::Jackcess(
            "Unknown encoded attachment data format".into(),
        ));
    }
    let type_flag = i32::from_le_bytes(encoded[0..4].try_into().unwrap());
    let data_len = i32::from_le_bytes(encoded[4..8].try_into().unwrap()) as usize;

    // raw content is used as is, compressed content is a zlib stream
    let content: Vec<u8> = match type_flag {
        DATA_TYPE_RAW => encoded[WRAPPER_HEADER_SIZE..].to_vec(),
        DATA_TYPE_COMPRESSED => crate::deflate::zlib_decompress(&encoded[WRAPPER_HEADER_SIZE..])?,
        other => {
            return Err(Error::Jackcess(format!(
                "Unknown encoded attachment data type {}",
                other
            )))
        }
    };

    // the content header is the header length, the CONTENT_HEADER_FLAG, the
    // character count of the extension which follows (and which is already a
    // separate field of the attachment table), so skip it all
    if content.len() < 4 {
        return Err(Error::Jackcess("invalid encoded attachment".into()));
    }
    let header_len = i32::from_le_bytes(content[0..4].try_into().unwrap());
    if header_len < 4 {
        return Err(Error::Jackcess("invalid encoded attachment".into()));
    }
    let header_len = header_len as usize;
    // the header length includes the bytes of the length field itself; the
    // stored data length covers the header and the file data
    if data_len < header_len || header_len > content.len() || data_len > content.len() {
        return Err(Error::Jackcess("invalid encoded attachment".into()));
    }
    Ok(content[header_len..data_len].to_vec())
}

// ---------------------------------------------------------------------------
// the foreign key handle
// ---------------------------------------------------------------------------

/// `ComplexValueForeignKey`: the id a main table row holds for a complex
/// column, plus the operations on the values behind it.
pub struct ComplexValueForeignKey<'a> {
    db: &'a mut Database,
    pub table_page: i32,
    pub row_id: RowId,
    pub info: ComplexColumnInfo,
    value: i32,
}

impl<'a> ComplexValueForeignKey<'a> {
    pub fn new(
        db: &'a mut Database,
        table_page: i32,
        row_id: RowId,
        info: ComplexColumnInfo,
        value: i32,
    ) -> ComplexValueForeignKey<'a> {
        ComplexValueForeignKey {
            db,
            table_page,
            row_id,
            info,
            value,
        }
    }

    pub fn get(&self) -> i32 {
        self.value
    }

    pub fn complex_type(&self) -> ComplexDataType {
        self.info.kind
    }

    pub fn count_values(&mut self) -> Result<usize> {
        self.info.count_values(self.db, self.value)
    }

    pub fn values(&mut self) -> Result<Vec<ComplexValue>> {
        self.info.values(self.db, self.value)
    }

    /// The multi-value values (`getMultiValues`).
    pub fn multi_values(&mut self) -> Result<Vec<ComplexValue>> {
        if self.info.kind != ComplexDataType::MultiValue {
            return Err(Error::IllegalState(
                "the column does not hold multi-values".into(),
            ));
        }
        self.values()
    }

    /// The attachments (`getAttachments`).
    pub fn attachments(&mut self) -> Result<Vec<ComplexValue>> {
        if self.info.kind != ComplexDataType::Attachment {
            return Err(Error::IllegalState(
                "the column does not hold attachments".into(),
            ));
        }
        self.values()
    }

    /// The name of the memo column a version history column tracks
    /// (`VersionHistoryColumnInfo.getValueColumn().getName()`).
    pub fn versioned_column_name(&self) -> Option<&str> {
        if self.info.kind != ComplexDataType::VersionHistory {
            return None;
        }
        self.info.value_column.as_deref()
    }

    /// The versions of the memo value (`getVersions`), newest first
    /// (`VersionHistoryColumnInfoImpl.toValues` sorts them by descending
    /// modified date, then descending id and complex value foreign key).
    pub fn versions(&mut self) -> Result<Vec<Version>> {
        if self.info.kind != ComplexDataType::VersionHistory {
            return Err(Error::IllegalState(
                "the column does not keep a version history".into(),
            ));
        }
        let raw = self.info.raw_values(self.db, self.value)?;
        let mut out = Vec::with_capacity(raw.len());
        for (row_id, row) in raw {
            let id = self.info.value_id(self.db, &row)?;
            let table = self.db.tables.get(&self.info.flat_table_page).unwrap();
            let text = |name: Option<&String>| -> Option<String> {
                name.and_then(|name| table.column_index(name))
                    .and_then(|i| row.get(i))
                    .and_then(|v| match v {
                        Value::Text(s) => Some(s.clone()),
                        _ => None,
                    })
            };
            let modified_date = self
                .info
                .modified_column
                .as_ref()
                .and_then(|name| table.column_index(name))
                .and_then(|i| row.get(i))
                .and_then(|v| match v {
                    Value::DateTime(ldt) => Some(*ldt),
                    _ => None,
                });
            out.push(Version {
                id,
                row_id,
                complex_value_fk: self.value,
                value: text(self.info.value_column.as_ref()),
                modified_date,
            });
        }
        // newest first: descending date, then descending id, then descending
        // complex value foreign key (`VersionImpl.compareTo`)
        out.sort_by(|a, b| {
            b.modified_date
                .cmp(&a.modified_date)
                .then_with(|| b.id.cmp(&a.id))
                .then_with(|| b.complex_value_fk.cmp(&a.complex_value_fk))
        });
        Ok(out)
    }

    /// `ComplexValueForeignKeyImpl.addVersion(value, modifiedDate)`: writes one
    /// version row to the flat table.  The id of the new row is its flat table
    /// auto number.
    pub fn add_version(&mut self, value: &str, modified_date: LocalDateTime) -> Result<Version> {
        if self.info.kind != ComplexDataType::VersionHistory {
            return Err(Error::IllegalState(
                "the column does not keep a version history".into(),
            ));
        }
        let value_column =
            self.info.value_column.clone().ok_or_else(|| {
                Error::IllegalState("version history value column missing".into())
            })?;
        let modified_column = self
            .info
            .modified_column
            .clone()
            .ok_or_else(|| Error::IllegalState("version history date column missing".into()))?;
        let fk = self.value;
        let mut row = self.empty_flat_row()?;
        self.set_flat_column(&mut row, &self.info.fk_column.clone(), Value::Long(fk))?;
        self.set_flat_column(&mut row, &value_column, Value::Text(value.to_string()))?;
        self.set_flat_column(&mut row, &modified_column, Value::DateTime(modified_date))?;
        let row_id = self.add_flat_row(row)?;
        let row = self.read_flat_row(row_id)?;
        let id = self.info.value_id(self.db, &row)?;
        Ok(Version {
            id,
            row_id,
            complex_value_fk: fk,
            value: Some(value.to_string()),
            modified_date: Some(modified_date),
        })
    }

    /// `ComplexValueForeignKeyImpl.addVersion(value)`: writes a version whose
    /// modified date is the database's current time (the port has only the
    /// `LocalDateTime` date type, not `java.util.Date`).
    pub fn add_version_now(&mut self, value: &str) -> Result<Version> {
        let now = self.db.now_local_datetime();
        self.add_version(value, now)
    }

    /// `VersionHistoryColumnInfoImpl.updateValue`: the strongly typed update of
    /// a version is not supported (the data is meant to be immutable).
    pub fn update_version(&mut self, _version: &Version) -> Result<()> {
        Err(Error::Unsupported(
            "This column does not support value updates".into(),
        ))
    }

    /// `VersionHistoryColumnInfoImpl.deleteValue`: the strongly typed delete of
    /// a version is not supported either, exactly like jackcess.
    pub fn delete_version(&mut self, _version: &Version) -> Result<()> {
        Err(Error::Unsupported(
            "This column does not support value deletes".into(),
        ))
    }

    fn empty_flat_row(&self) -> Result<Vec<Value>> {
        let len = self
            .db
            .tables
            .get(&self.info.flat_table_page)
            .map(|t| t.columns.len())
            .ok_or_else(|| Error::IllegalState("flat table is not loaded".into()))?;
        Ok(vec![Value::Null; len])
    }

    fn set_flat_column(&self, row: &mut [Value], name: &str, value: Value) -> Result<()> {
        let table = self.db.tables.get(&self.info.flat_table_page).unwrap();
        match table.column_index(name) {
            Some(idx) if idx < row.len() => {
                row[idx] = value;
                Ok(())
            }
            _ => Err(Error::IllegalState(format!(
                "flat table has no column {}",
                name
            ))),
        }
    }

    fn add_flat_row(&mut self, row: Vec<Value>) -> Result<RowId> {
        let page = self.info.flat_table_page;
        let tz = self.db.tz;
        let table = self.db.tables.get_mut(&page).unwrap();
        let ids = table.add_rows_with_ids(&mut self.db.channel, &[row], &tz)?;
        Ok(ids[0])
    }

    fn read_flat_row(&mut self, row_id: RowId) -> Result<Vec<Value>> {
        let page = self.info.flat_table_page;
        let table = self.db.tables.get(&page).unwrap();
        table.read_row_at(&mut self.db.channel, row_id)
    }

    /// `ComplexValueForeignKeyImpl.addMultiValue`.
    pub fn add_multi_value(&mut self, value: Value) -> Result<ComplexValue> {
        if self.info.kind != ComplexDataType::MultiValue {
            return Err(Error::IllegalState(
                "the column does not hold multi-values".into(),
            ));
        }
        let fk = self.value;
        let mut row = self.empty_flat_row()?;
        self.set_flat_column(&mut row, &self.info.fk_column.clone(), Value::Long(fk))?;
        let vc = self.info.value_column.clone().unwrap_or_default();
        self.set_flat_column(&mut row, &vc, value)?;
        let row_id = self.add_flat_row(row)?;
        let row = self.read_flat_row(row_id)?;
        let id = self.info.value_id(self.db, &row)?;
        let table = self.db.tables.get(&self.info.flat_table_page).unwrap();
        let out_value = table
            .column_index(&self.info.value_column.clone().unwrap_or_default())
            .and_then(|i| row.get(i).cloned())
            .unwrap_or(Value::Null);
        Ok(ComplexValue {
            id,
            row_id,
            complex_value_fk: fk,
            value: out_value,
            attachment: None,
        })
    }

    /// `add_single` is the multi-value `add` of the Java API.
    pub fn add_single(&mut self, value: Value) -> Result<ComplexValue> {
        self.add_multi_value(value)
    }

    /// `update_single` is the multi-value `update` of the Java API.
    pub fn update_single(&mut self, value: &ComplexValue) -> Result<()> {
        self.update_multi_value(value)
    }

    /// `delete_single` is the multi-value `delete` of the Java API.
    pub fn delete_single(&mut self, value: &ComplexValue) -> Result<()> {
        self.delete_value(value)
    }

    /// `ComplexValueForeignKeyImpl.updateMultiValue`.
    pub fn update_multi_value(&mut self, value: &ComplexValue) -> Result<()> {
        if self.info.kind == ComplexDataType::VersionHistory {
            // `VersionHistoryColumnInfoImpl.updateValue`
            return Err(Error::Unsupported(
                "This column does not support value updates".into(),
            ));
        }
        let fk = self.value;
        let mut row = self.empty_flat_row()?;
        self.set_flat_column(
            &mut row,
            &self.info.pk_column.clone(),
            Value::Long(value.id),
        )?;
        self.set_flat_column(&mut row, &self.info.fk_column.clone(), Value::Long(fk))?;
        let vc = self.info.value_column.clone().unwrap_or_default();
        self.set_flat_column(&mut row, &vc, value.value.clone())?;
        let page = self.info.flat_table_page;
        let tz = self.db.tz;
        let table = self.db.tables.get_mut(&page).unwrap();
        table.update_row(&mut self.db.channel, value.row_id, &row, &tz)
    }

    /// `ComplexColumnInfoImpl.deleteValue`.
    pub fn delete_value(&mut self, value: &ComplexValue) -> Result<()> {
        if self.info.kind == ComplexDataType::VersionHistory {
            // `VersionHistoryColumnInfoImpl.deleteValue`
            return Err(Error::Unsupported(
                "This column does not support value deletes".into(),
            ));
        }
        let page = self.info.flat_table_page;
        let table = self.db.tables.get_mut(&page).unwrap();
        table.delete_row(&mut self.db.channel, value.row_id)
    }

    /// `ComplexColumnInfoImpl.deleteAllValues`.
    pub fn delete_all_values(&mut self) -> Result<()> {
        if self.info.kind == ComplexDataType::VersionHistory {
            // `VersionHistoryColumnInfoImpl.deleteAllValues`
            return Err(Error::Unsupported(
                "This column does not support value deletes".into(),
            ));
        }
        let values = self.values()?;
        for v in values {
            self.delete_value(&v)?;
        }
        Ok(())
    }

    /// `ComplexValueForeignKeyImpl.addAttachment`: encodes the file data the way
    /// access stores it (raw for the types it keeps raw, deflate compressed
    /// otherwise).
    #[allow(clippy::too_many_arguments)]
    pub fn add_attachment(
        &mut self,
        url: Option<&str>,
        name: Option<&str>,
        file_type: Option<&str>,
        data: &[u8],
        timestamp: Option<LocalDateTime>,
        flags: Option<i32>,
    ) -> Result<AttachmentValue> {
        if self.info.kind != ComplexDataType::Attachment {
            return Err(Error::IllegalState(
                "the column does not hold attachments".into(),
            ));
        }
        let att = self
            .info
            .attachment
            .clone()
            .ok_or_else(|| Error::IllegalState("attachment columns missing".into()))?;
        let encoded = encode_attachment_data(file_type.unwrap_or(""), data)?;
        let fk = self.value;
        let mut row = self.empty_flat_row()?;
        self.set_flat_column(&mut row, &self.info.fk_column.clone(), Value::Long(fk))?;
        self.set_flat_column(
            &mut row,
            &att.url,
            url.map(|s| Value::Text(s.to_string()))
                .unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.name,
            name.map(|s| Value::Text(s.to_string()))
                .unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.file_type,
            file_type
                .map(|s| Value::Text(s.to_string()))
                .unwrap_or(Value::Null),
        )?;
        self.set_flat_column(&mut row, &att.data, Value::Bytes(encoded))?;
        self.set_flat_column(
            &mut row,
            &att.timestamp,
            timestamp.map(Value::DateTime).unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.flags,
            flags.map(Value::Long).unwrap_or(Value::Null),
        )?;
        let row_id = self.add_flat_row(row)?;
        let row = self.read_flat_row(row_id)?;
        let id = self.info.value_id(self.db, &row)?;
        Ok(AttachmentValue {
            id,
            row_id,
            url: url.map(|s| s.to_string()),
            name: name.map(|s| s.to_string()),
            file_type: file_type.map(|s| s.to_string()),
            data: Some(data.to_vec()),
            encoded_data: None,
            timestamp,
            flags,
        })
    }

    /// `ComplexValueForeignKeyImpl.updateAttachment`.
    pub fn update_attachment(&mut self, attachment: &AttachmentValue) -> Result<()> {
        let att = self
            .info
            .attachment
            .clone()
            .ok_or_else(|| Error::IllegalState("attachment columns missing".into()))?;
        let encoded = match &attachment.data {
            Some(data) => Some(encode_attachment_data(
                attachment.file_type.as_deref().unwrap_or(""),
                data,
            )?),
            None => attachment.encoded_data.clone(),
        };
        let fk = self.value;
        let mut row = self.empty_flat_row()?;
        self.set_flat_column(
            &mut row,
            &self.info.pk_column.clone(),
            Value::Long(attachment.id),
        )?;
        self.set_flat_column(&mut row, &self.info.fk_column.clone(), Value::Long(fk))?;
        self.set_flat_column(
            &mut row,
            &att.url,
            attachment
                .url
                .as_ref()
                .map(|s| Value::Text(s.clone()))
                .unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.name,
            attachment
                .name
                .as_ref()
                .map(|s| Value::Text(s.clone()))
                .unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.file_type,
            attachment
                .file_type
                .as_ref()
                .map(|s| Value::Text(s.clone()))
                .unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.data,
            encoded.map(Value::Bytes).unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.timestamp,
            attachment
                .timestamp
                .map(Value::DateTime)
                .unwrap_or(Value::Null),
        )?;
        self.set_flat_column(
            &mut row,
            &att.flags,
            attachment.flags.map(Value::Long).unwrap_or(Value::Null),
        )?;
        let page = self.info.flat_table_page;
        let tz = self.db.tz;
        let table = self.db.tables.get_mut(&page).unwrap();
        table.update_row(&mut self.db.channel, attachment.row_id, &row, &tz)
    }

    /// `ComplexValueForeignKeyImpl.deleteAttachment`.
    pub fn delete_attachment(&mut self, attachment: &AttachmentValue) -> Result<()> {
        let page = self.info.flat_table_page;
        let table = self.db.tables.get_mut(&page).unwrap();
        table.delete_row(&mut self.db.channel, attachment.row_id)
    }
}

// ---------------------------------------------------------------------------
// creating the supporting tables
// ---------------------------------------------------------------------------

/// `ComplexColumnCreator.newVersionHistoryColumn`: the complex column which
/// holds the versions of a memo column.  A caller never names or sees it, and
/// asks for it with `ColumnSpec::set_append_only`, so the table creator and the
/// updater build it here.
pub fn version_history_column_spec(memo_column_name: &str) -> ColumnSpec {
    let mut spec = ColumnSpec::new(VERSION_HISTORY_COL_NAME, DataType::ComplexType);
    spec.auto_number = true;
    spec.complex_desc = Some(ComplexColumnDesc::version_history(memo_column_name));
    spec
}

/// `ComplexColumnCreator.copyValueColumn`.
pub fn copy_value_column(type_col: &Column) -> ColumnSpec {
    let mut spec = ColumnSpec::new(type_col.name.clone(), type_col.data_type);
    if type_col.data_type.is_textual() {
        spec.length = Some(type_col.column_length);
    }
    spec.compressed_unicode = type_col.compressed_unicode;
    if type_col.data_type.has_scale_precision() {
        spec.precision = Some(type_col.precision);
        spec.scale = Some(type_col.scale);
    }
    spec
}

/// Builds the flat table of a complex column (`ComplexColumnCreator.
/// createFlatTable`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_flat_table_spec(
    db: &mut Database,
    parent_name: &str,
    col: &ColumnSpec,
    desc: &ComplexColumnDesc,
    type_table_page: i32,
    flat_table_name: String,
    format: &crate::format::JetFormat,
) -> Result<crate::tablecreator::TableSpec> {
    let fk_name = truncate(&format!("_{}", col.name), format.max_column_name_length);
    let pk_name = truncate(
        &format!("{}_{}", parent_name, col.name),
        format.max_column_name_length,
    );
    let mut fk_col = ColumnSpec::new(fk_name.clone(), DataType::Long);
    fk_col.extra_flags = COMPLEX_FK_EXT_FLAG_MASK;
    let mut pk_col = ColumnSpec::new(pk_name.clone(), DataType::Long);
    pk_col.auto_number = true;

    let mut columns = vec![fk_col];
    if desc.kind == ComplexDataType::MultiValue {
        columns.push(pk_col.clone());
    }
    let type_cols: Vec<Column> = db
        .tables
        .get(&type_table_page)
        .map(|t| t.columns.clone())
        .unwrap_or_default();
    for type_col in &type_cols {
        let mut value_col = copy_value_column(type_col);
        if desc.kind == ComplexDataType::MultiValue && desc.row_source.is_some() {
            value_col = value_col
                .put_property_typed(
                    crate::property::ROW_SOURCE_TYPE_PROP,
                    DataType::Text,
                    Value::Text(desc.row_source_type.clone().unwrap_or_default()),
                )
                .put_property_typed(
                    crate::property::ROW_SOURCE_PROP,
                    DataType::Memo,
                    Value::Text(desc.row_source.clone().unwrap_or_default()),
                )
                .put_property_typed(
                    crate::property::COLUMN_COUNT_PROP,
                    DataType::Int,
                    Value::Int(desc.column_count as i16),
                )
                .put_property_typed(
                    crate::property::BOUND_COLUMN_PROP,
                    DataType::Int,
                    Value::Int(1),
                );
        }
        columns.push(value_col);
    }
    if desc.kind != ComplexDataType::MultiValue {
        columns.push(pk_col);
    }

    let mut spec = crate::tablecreator::TableSpec::new(flat_table_name, columns);
    spec.indexes
        .push(crate::tablecreator::IndexSpec::primary_key(
            FLAT_PK_INDEX_NAME,
            vec![(pk_name.clone(), true)],
        ));
    spec.indexes.push(crate::tablecreator::IndexSpec::new(
        truncate(&fk_name, format.max_index_name_length),
        vec![(fk_name, true)],
    ));
    Ok(spec)
}

/// `StringUtil.truncate`: cut a name at the maximum length (UTF-16 units).
pub fn truncate(name: &str, max_len: usize) -> String {
    let units: Vec<u16> = name.encode_utf16().collect();
    if units.len() <= max_len {
        return name.to_string();
    }
    String::from_utf16_lossy(&units[..max_len])
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::format::FileFormat;
    use crate::tablecreator::{IndexSpec, TableSpec};

    /// `resources/complex_tdefs.bin`: the table definition page of the Java
    /// widow's `Test` table (multi-value column) followed by the definition
    /// page of its flat table, generated by `java-src/ComplexFixtures.java`.
    const FIXTURE: &[u8] = include_bytes!("../resources/complex_tdefs.bin");
    /// the (random) flat table name in the fixture; reused so the definition
    /// page bytes are the same
    const FIXTURE_FLAT_NAME: &str = include_str!("../resources/complex_flat_name.txt");

    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn fixture_spec() -> TableSpec {
        TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
                ColumnSpec::new("Tags", DataType::Text)
                    .set_multi_value(DataType::Text)
                    .set_value_list(&["a", "b", "c"]),
            ],
        )
    }

    fn create_fixture_db(name: &str) -> (std::path::PathBuf, Database) {
        let path = test_tmp_dir().join(format!(
            "jackcess_complex_{}_{}.accdb",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        db.set_complex_names(vec![FIXTURE_FLAT_NAME.trim().to_string()]);
        db.create_table(&fixture_spec()).expect("create table");
        (path, db)
    }

    /// The complex column definition (the `COMPLEX_TYPE` type byte and the
    /// complex id in the four bytes a text column uses for its sort order), the
    /// flat table's foreign key flag and the definitions of both tables match
    /// the bytes the Java library wrote.
    #[test]
    fn complex_column_definition_matches_java_fixture() {
        let (path, mut db) = create_fixture_db("defs");
        db.add_rows("Test", &[vec![Value::Null, Value::Text("one".to_string())]])
            .expect("add row");
        db.flush().expect("flush");

        let test_page = db.table_lookup.get("TEST").expect("Test lookup").0;
        let flat_name = FIXTURE_FLAT_NAME.trim().to_uppercase();
        let flat_page = db
            .table_lookup
            .get(&flat_name)
            .expect("flat table lookup")
            .0;
        let (test_flags, flat_flags) = (
            db.table_lookup.get("TEST").unwrap().1,
            db.table_lookup.get(&flat_name).unwrap().1,
        );
        assert_eq!(test_page, 80, "Test tdef page");
        assert_eq!(flat_page, 82, "flat tdef page");
        assert_eq!(test_flags, CATALOG_FLAGS_HAS_COMPLEX, "Test catalog flags");
        assert_eq!(flat_flags, CATALOG_FLAGS_FLAT_TABLE, "flat catalog flags");

        let bytes = std::fs::read(&path).expect("read database file");
        let page = |n: i32| &bytes[n as usize * 4096..(n as usize + 1) * 4096];
        assert_eq!(page(test_page), &FIXTURE[..4096], "Test table definition");
        assert_eq!(page(flat_page), &FIXTURE[4096..], "flat table definition");

        // the definition blocks carry the values the Java library writes
        let format = db.format.clone();
        let test = db.tables.get(&test_page).expect("Test loaded");
        let tags = test.get_column("Tags").expect("Tags column");
        assert_eq!(tags.data_type, DataType::ComplexType);
        assert_eq!(tags.complex_id, 1, "MSysComplexColumns id");
        assert!(tags.auto_number);
        assert_eq!(
            test.last_complex_auto_number, 1,
            "the row consumed one complex id"
        );

        let flat = db.tables.get(&flat_page).expect("flat table loaded");
        let fk = flat
            .columns
            .iter()
            .find(|c| c.complex_value_fk)
            .expect("flat foreign key column");
        assert_eq!(fk.name, "_Tags");
        assert_eq!(fk.ext_flags & COMPLEX_FK_EXT_FLAG_MASK, 0x08);
        assert_eq!(flat.columns.len(), 3);
        assert_eq!(flat.indexes.len(), 2);
        assert_eq!(flat.indexes[0].name, FLAT_PK_INDEX_NAME);
        assert_eq!(flat.indexes[1].name, "_Tags");

        // the row and the MSysComplexColumns row point at the supporting tables
        let row = db
            .read_rows_with_ids("Test")
            .expect("rows")
            .into_iter()
            .next()
            .and_then(|(_, values)| values)
            .expect("row");
        assert_eq!(row[2], Value::Long(1), "row holds the complex id");
        let cc = db.complex_info("Test", "Tags").expect("complex info");
        assert_eq!(cc.kind, ComplexDataType::MultiValue);
        assert_eq!(cc.value_column.as_deref(), Some("Value"));
        let cc_page = db
            .table_lookup
            .get(&COMPLEX_COLUMNS_TABLE.to_uppercase())
            .expect("MSysComplexColumns lookup")
            .0;
        let complex_columns = db
            .read_table_rows(cc_page)
            .expect("MSysComplexColumns rows");
        let (idx_id, idx_table, idx_type, idx_flat, idx_name) = {
            let table = db.tables.get(&cc_page).unwrap();
            (
                table.column_index(COL_COMPLEX_ID).unwrap(),
                table.column_index(COL_TABLE_ID).unwrap(),
                table.column_index(COL_COMPLEX_TYPE_OBJECT_ID).unwrap(),
                table.column_index(COL_FLAT_TABLE_ID).unwrap(),
                table.column_index(COL_COLUMN_NAME).unwrap(),
            )
        };
        let cc_row = complex_columns
            .into_iter()
            .flatten()
            .find(|r| r[idx_id].as_i64() == Some(1))
            .expect("complex column row");
        assert_eq!(cc_row[idx_table], Value::Long(test_page));
        assert_eq!(cc_row[idx_type], Value::Long(37)); // MSysComplexType_Text
        assert_eq!(cc_row[idx_flat], Value::Long(flat_page));
        assert_eq!(cc_row[idx_name], Value::Text("Tags".to_string()));

        let _ = format;
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `add_multi_value` / `update_multi_value` / `delete_value` /
    /// `multi_values` round trip through the flat table, also after reopening
    /// the database.
    #[test]
    fn multi_value_api_round_trips() {
        let (path, mut db) = create_fixture_db("api");
        db.add_rows("Test", &[vec![Value::Null, Value::Text("one".to_string())]])
            .expect("add row");
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();
        {
            let mut fk = db
                .complex_value_fk("Test", row_id, "Tags")
                .expect("complex fk");
            assert_eq!(fk.get(), 1);
            assert_eq!(fk.count_values().expect("count"), 0);
            let a = fk
                .add_multi_value(Value::Text("a".to_string()))
                .expect("add a");
            let b = fk
                .add_multi_value(Value::Text("b".to_string()))
                .expect("add b");
            assert_eq!(a.id, 1);
            assert_eq!(b.id, 2);
            assert_eq!(fk.count_values().expect("count"), 2);
        }
        db.flush().expect("flush");
        drop(db);

        let mut db = Database::open_path(&path).expect("reopen");
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();
        {
            let mut fk = db
                .complex_value_fk("Test", row_id, "Tags")
                .expect("complex fk");
            let mut values = fk.multi_values().expect("values");
            assert_eq!(values.len(), 2);
            assert_eq!(values[0].value, Value::Text("a".to_string()));
            assert_eq!(values[1].value, Value::Text("b".to_string()));
            values[0].value = Value::Text("A".to_string());
            fk.update_multi_value(&values[0]).expect("update");
            fk.delete_value(&values[1]).expect("delete");
            let values = fk.multi_values().expect("values after update");
            assert_eq!(values.len(), 1);
            assert_eq!(values[0].value, Value::Text("A".to_string()));
        }
        db.flush().expect("flush");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `AttachmentImpl.encodeData` / `decodeData`: the wrapper header, the
    /// content header and the data, raw for the types access stores raw and
    /// deflate compressed otherwise.
    #[test]
    fn attachment_data_wrapper_round_trips() {
        let data = b"hello attachment";
        let encoded = encode_attachment_data("png", data).expect("encode");
        assert_eq!(&encoded[0..4], &0i32.to_le_bytes(), "raw type flag");
        assert_eq!(
            &encoded[4..8],
            &((data.len() as i32) + 12 + 2 * 4).to_le_bytes(),
            "content length"
        );
        assert_eq!(&encoded[8..12], &(12i32 + 8).to_le_bytes(), "header len");
        assert_eq!(&encoded[12..16], &1i32.to_le_bytes(), "header flag");
        assert_eq!(&encoded[16..20], &4i32.to_le_bytes(), "extension chars");
        assert_eq!(
            &encoded[20..28],
            "png\0"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<u8>>()
        );
        assert_eq!(&encoded[28..], data);
        assert_eq!(decode_attachment_data(&encoded).expect("decode"), data);

        // a compressed type carries the same wrapper header (with the
        // uncompressed length) and a zlib stream after it
        let encoded = encode_attachment_data("txt", data).expect("encode");
        assert_eq!(&encoded[0..4], &1i32.to_le_bytes(), "compressed flag");
        assert_eq!(
            &encoded[4..8],
            &((data.len() as i32) + 12 + 2 * 4).to_le_bytes(),
            "uncompressed content length"
        );
        assert_eq!(&encoded[8..10], &[0x78, 0x5e], "zlib header");
        assert_eq!(decode_attachment_data(&encoded).expect("decode"), data);

        // the file extension is lower cased, like jackcess
        let upper = encode_attachment_data("PNG", data).expect("encode");
        assert_eq!(
            &upper[20..28],
            "png\0"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes())
                .collect::<Vec<u8>>()
        );
        assert_eq!(&upper[0..4], &0i32.to_le_bytes());
        let upper = encode_attachment_data("TXT", data).expect("encode");
        assert_eq!(&upper[0..4], &1i32.to_le_bytes());
        assert_eq!(decode_attachment_data(&upper).expect("decode"), data);
    }

    /// `resources/attach_ref/attachment_corpus.bin`: whole encoded attachment
    /// payloads produced by `AttachmentColumnInfoImpl.newAttachment(...)
    /// .getEncodedFileData()` in `java-src/AttachOracle.java`, for raw and
    /// compressed file types, empty and non-empty data.
    const ATTACHMENT_CORPUS: &[u8] =
        include_bytes!("../resources/attach_ref/attachment_corpus.bin");

    #[test]
    fn attachment_encoding_matches_java() {
        let data = ATTACHMENT_CORPUS;
        assert_eq!(&data[0..4], b"AT01", "corpus magic");
        let mut pos = 4;
        let be_u16 = |data: &[u8], pos: &mut usize| -> usize {
            let v = u16::from_be_bytes([data[*pos], data[*pos + 1]]) as usize;
            *pos += 2;
            v
        };
        let be_u32 = |data: &[u8], pos: &mut usize| -> usize {
            let v = u32::from_be_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]])
                as usize;
            *pos += 4;
            v
        };
        let count = be_u32(data, &mut pos);
        assert!(count >= 90, "corpus is unexpectedly small");
        let mut failures = Vec::new();
        for _ in 0..count {
            let name_len = be_u16(data, &mut pos);
            let name = String::from_utf8(data[pos..pos + name_len].to_vec()).unwrap();
            pos += name_len;
            let type_len = be_u16(data, &mut pos);
            let file_type = String::from_utf8(data[pos..pos + type_len].to_vec()).unwrap();
            pos += type_len;
            let data_len = be_u32(data, &mut pos);
            let file_data = data[pos..pos + data_len].to_vec();
            pos += data_len;
            let encoded_len = be_u32(data, &mut pos);
            let expected = data[pos..pos + encoded_len].to_vec();
            pos += encoded_len;

            match encode_attachment_data(&file_type, &file_data) {
                Ok(got) => {
                    if got != expected {
                        let at = got
                            .iter()
                            .zip(expected.iter())
                            .position(|(a, b)| a != b)
                            .unwrap_or(got.len().min(expected.len()));
                        failures.push(format!(
                            "{}: got {} bytes, want {} bytes, first difference at {}",
                            name,
                            got.len(),
                            expected.len(),
                            at
                        ));
                    }
                }
                Err(e) => failures.push(format!("{}: error {}", name, e)),
            }
            assert_eq!(
                decode_attachment_data(&expected).expect("decode"),
                file_data,
                "decoding {}",
                name
            );
        }
        assert_eq!(pos, data.len(), "corpus has trailing bytes");
        assert!(
            failures.is_empty(),
            "{} of {} attachment corpus entries differ:\n{}",
            failures.len(),
            count,
            failures.join("\n")
        );
    }

    /// The flat table of an attachment column is shaped like the
    /// `MSysComplexType_Attachment` table, with the pk after the value columns.
    #[test]
    fn attachment_column_creates_the_expected_flat_table() {
        let path =
            test_tmp_dir().join(format!("jackcess_complex_att_{}.accdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        db.set_complex_names(vec!["f_00000000000000000000000000000001_Files".to_string()]);
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
                ColumnSpec::new("Files", DataType::Text).set_attachment(),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).expect("create table");
        db.add_rows("Test", &[vec![Value::Null, Value::Text("one".to_string())]])
            .expect("add row");
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();
        {
            let mut fk = db
                .complex_value_fk("Test", row_id, "Files")
                .expect("complex fk");
            assert_eq!(fk.complex_type(), ComplexDataType::Attachment);
            let att = fk
                .add_attachment(
                    None,
                    Some("photo.png"),
                    Some("png"),
                    &[1u8, 2, 3, 4],
                    Some(LocalDateTime::new(2021, 5, 4, 12, 34, 56, 0)),
                    Some(0),
                )
                .expect("add attachment");
            assert_eq!(att.id, 1);
            let values = fk.attachments().expect("attachments");
            assert_eq!(values.len(), 1);
            let a = values[0].attachment.as_ref().expect("attachment value");
            assert_eq!(a.name.as_deref(), Some("photo.png"));
            assert_eq!(a.file_type.as_deref(), Some("png"));
            assert_eq!(a.data.as_deref(), Some(&[1u8, 2, 3, 4][..]));
        }
        db.flush().expect("flush");
        drop(db);

        let mut db = Database::open_path(&path).expect("reopen");
        let info = db.complex_info("Test", "Files").expect("complex info");
        let flat = db.tables.get(&info.flat_table_page).expect("flat table");
        let names: Vec<&str> = flat.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "_Files",
                "FileURL",
                "FileName",
                "FileType",
                "FileData",
                "FileTimeStamp",
                "FileFlags",
                "Test_Files"
            ]
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Attachments of the file types access deflates are compressed on write and
    /// inflated on read, through the real flat table.
    #[test]
    fn attachment_compressed_data_round_trips_through_the_database() {
        let path = test_tmp_dir().join(format!(
            "jackcess_complex_att_zip_{}.accdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        db.set_complex_names(vec!["f_00000000000000000000000000000002_Files".to_string()]);
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
                ColumnSpec::new("Files", DataType::Text).set_attachment(),
            ],
        );
        db.create_table(&spec).expect("create table");
        db.add_rows("Test", &[vec![Value::Null, Value::Text("one".to_string())]])
            .expect("add row");
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();

        let text: Vec<u8> = (0..70000usize)
            .map(|i| b"the quick brown fox "[i % 20])
            .collect();
        let binary: Vec<u8> = (0..5000u32)
            .map(|i| (i.wrapping_mul(1103515245).wrapping_add(12345) >> 16) as u8)
            .collect();
        {
            let mut fk = db
                .complex_value_fk("Test", row_id, "Files")
                .expect("complex fk");
            fk.add_attachment(None, Some("note.txt"), Some("txt"), &text, None, Some(0))
                .expect("add txt");
            fk.add_attachment(None, Some("blob.bin"), Some("bin"), &binary, None, Some(0))
                .expect("add bin");
            fk.add_attachment(
                None,
                Some("photo.png"),
                Some("png"),
                &binary[..100],
                None,
                Some(0),
            )
            .expect("add png");

            // the stored wrappers carry the type flag access uses: 1 for the
            // deflated types, 0 for the raw ones
            let values = fk.attachments().expect("attachments");
            let flags: Vec<i32> = values
                .iter()
                .map(|v| {
                    let encoded = v
                        .attachment
                        .as_ref()
                        .unwrap()
                        .encoded_data
                        .as_ref()
                        .unwrap();
                    i32::from_le_bytes(encoded[0..4].try_into().unwrap())
                })
                .collect();
            assert_eq!(flags, vec![1, 1, 0]);
        }
        db.flush().expect("flush");
        drop(db);

        let mut db = Database::open_path(&path).expect("reopen");
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();
        let mut fk = db
            .complex_value_fk("Test", row_id, "Files")
            .expect("complex fk");
        let values = fk.attachments().expect("attachments");
        assert_eq!(values.len(), 3);
        assert_eq!(
            values[0].attachment.as_ref().unwrap().data.as_deref(),
            Some(&text[..])
        );
        assert_eq!(
            values[1].attachment.as_ref().unwrap().data.as_deref(),
            Some(&binary[..])
        );
        assert_eq!(
            values[2].attachment.as_ref().unwrap().data.as_deref(),
            Some(&binary[..100])
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `ColumnBuilder.addToTable` with a complex column: the supporting table
    /// and the `MSysComplexColumns` row are created, the table's catalog flags
    /// gain `CATALOG_FLAGS_HAS_COMPLEX`, and the rows which already exist get
    /// the next complex ids.
    #[test]
    fn add_complex_column_to_an_existing_table() {
        let path = test_tmp_dir().join(format!(
            "jackcess_complex_addcol_{}.accdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        db.set_complex_names(vec!["f_00000000000000000000000000000002_Tags".to_string()]);
        let plain = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
            ],
        );
        db.create_table(&plain).expect("create table");
        for name in ["a", "b", "c"] {
            db.add_rows("Test", &[vec![Value::Null, Value::Text(name.to_string())]])
                .expect("add row");
        }
        let col = db
            .add_column(
                "Test",
                ColumnSpec::new("Tags", DataType::Text)
                    .set_multi_value(DataType::Text)
                    .set_value_list(&["x", "y"]),
            )
            .expect("add complex column");
        assert_eq!(col.data_type, DataType::ComplexType);
        assert_eq!(col.complex_id, 1);
        let page = db.table_lookup.get("TEST").expect("lookup").0;
        assert_eq!(
            db.table_lookup.get("TEST").unwrap().1 & CATALOG_FLAGS_HAS_COMPLEX,
            CATALOG_FLAGS_HAS_COMPLEX
        );
        {
            let table = db.tables.get(&page).unwrap();
            assert_eq!(table.columns.len(), 3);
            assert_eq!(table.last_complex_auto_number, 3);
        }
        let rows = db.read_rows_with_ids("Test").expect("rows");
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|(_, v)| v.as_ref())
            .filter_map(|v| v.get(2))
            .filter_map(|v| v.as_i64())
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);
        // the column is usable right away
        let (row_id, _) = rows[0].clone();
        {
            let mut fk = db
                .complex_value_fk("Test", row_id, "Tags")
                .expect("complex fk");
            assert_eq!(fk.get(), 1);
            fk.add_multi_value(Value::Text("x".to_string()))
                .expect("add value");
            assert_eq!(fk.count_values().expect("count"), 1);
        }
        db.flush().expect("flush");
        drop(db);

        let mut db = Database::open_path(&path).expect("reopen");
        let info = db.complex_info("Test", "Tags").expect("complex info");
        assert_eq!(info.kind, ComplexDataType::MultiValue);
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();
        let values = db.complex_values("Test", row_id, "Tags").expect("values");
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].value, Value::Text("x".to_string()));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `resources/complex_vh_tdefs.bin`: the definition page of the Java
    /// widow's version history `Test` table, followed by the definition page of
    /// its per-column type table and the one of its flat table, generated by
    /// `java-src/ComplexFixtures.java`.  The random names of the two supporting
    /// tables are stored in `resources/complex_vh_names.txt` (type table first).
    const VH_FIXTURE: &[u8] = include_bytes!("../resources/complex_vh_tdefs.bin");
    const VH_FIXTURE_NAMES: &str = include_str!("../resources/complex_vh_names.txt");

    fn vh_fixture_names() -> (String, String) {
        let names: Vec<&str> = VH_FIXTURE_NAMES
            .lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect();
        assert_eq!(names.len(), 2, "fixture names");
        (names[0].to_string(), names[1].to_string())
    }

    fn version_history_spec() -> TableSpec {
        TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(40),
                ColumnSpec::new("Body", DataType::Memo).set_append_only(true),
            ],
        )
    }

    /// The version history table definition, the one of its per-column type
    /// table (`MSysComplexTypeVH_<guid>`) and the one of its flat table match
    /// the bytes the Java library wrote, and the catalog flags, ext flag and
    /// flat table shape are the ones the Java library gives them.
    #[test]
    fn version_history_definitions_match_java_fixture() {
        let (type_name, flat_name) = vh_fixture_names();
        let path = test_tmp_dir().join(format!("jackcess_complex_vh_{}.accdb", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        db.set_complex_names(vec![type_name.clone(), flat_name.clone()]);
        db.create_table(&version_history_spec())
            .expect("create table");
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Text("one".to_string()),
                Value::Text("initial body".to_string()),
            ]],
        )
        .expect("add row");
        db.flush().expect("flush");

        let test_page = db.table_lookup.get("TEST").expect("Test lookup").0;
        let type_page = db
            .table_lookup
            .get(&type_name.to_uppercase())
            .expect("type table lookup")
            .0;
        let flat_page = db
            .table_lookup
            .get(&flat_name.to_uppercase())
            .expect("flat table lookup")
            .0;
        assert_eq!(test_page, 80, "Test tdef page");
        assert_eq!(type_page, 82, "type table tdef page");
        assert_eq!(flat_page, 84, "flat table tdef page");
        assert_eq!(
            db.table_lookup.get("TEST").unwrap().1,
            CATALOG_FLAGS_HAS_COMPLEX,
            "Test catalog flags"
        );
        assert_eq!(
            db.table_lookup.get(&type_name.to_uppercase()).unwrap().1,
            CATALOG_FLAGS_TYPE_TABLE,
            "type table catalog flags"
        );
        assert_eq!(
            db.table_lookup.get(&flat_name.to_uppercase()).unwrap().1,
            CATALOG_FLAGS_FLAT_TABLE,
            "flat table catalog flags"
        );

        let bytes = std::fs::read(&path).expect("read database file");
        let page = |n: i32| &bytes[n as usize * 4096..(n as usize + 1) * 4096];
        assert_eq!(
            page(test_page),
            &VH_FIXTURE[..4096],
            "Test table definition"
        );
        assert_eq!(
            page(type_page),
            &VH_FIXTURE[4096..8192],
            "version history type table definition"
        );
        assert_eq!(
            page(flat_page),
            &VH_FIXTURE[8192..],
            "version history flat table definition"
        );

        // the memo column, its version history column and the supporting
        // tables
        {
            let table = db.tables.get(&test_page).expect("Test loaded");
            assert_eq!(table.columns.len(), 4);
            let body = table.get_column("Body").expect("Body column");
            assert_eq!(body.data_type, DataType::Memo);
            let vh = table
                .get_column(VERSION_HISTORY_COL_NAME)
                .expect("version history column");
            assert_eq!(vh.data_type, DataType::ComplexType);
            assert!(vh.auto_number);
            assert_eq!(
                vh.ext_flags & crate::column::VERSION_HISTORY_EXT_FLAG_MASK,
                crate::column::VERSION_HISTORY_EXT_FLAG_MASK,
                "VERSION_HISTORY ext flag"
            );
        }
        assert_eq!(
            db.version_history_column_name("Test", "Body")
                .expect("version history column"),
            Some(VERSION_HISTORY_COL_NAME.to_string())
        );
        let info = db
            .version_history_info("Test", "Body")
            .expect("version history info")
            .expect("version history info present");
        assert_eq!(info.kind, ComplexDataType::VersionHistory);
        assert_eq!(info.value_column.as_deref(), Some("Body"));
        assert_eq!(
            info.modified_column.as_deref(),
            Some(MODIFIED_COL_NAME),
            "Modified column"
        );
        assert_eq!(info.flat_table_page, flat_page);
        assert_eq!(info.type_table_page, type_page);

        // the type table holds the memo column and the modified date
        {
            let type_table = db.tables.get(&type_page).expect("type table loaded");
            let names: Vec<&str> = type_table.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(names, vec!["Body", MODIFIED_COL_NAME]);
            assert_eq!(type_table.columns[0].data_type, DataType::Memo);
            assert_eq!(type_table.columns[1].data_type, DataType::ShortDateTime);
            assert_eq!(type_table.indexes.len(), 0);
        }

        // the flat table is fk, memo value, modified date, auto number key
        {
            let flat = db.tables.get(&flat_page).expect("flat table loaded");
            let names: Vec<&str> = flat.columns.iter().map(|c| c.name.as_str()).collect();
            assert_eq!(
                names,
                vec![
                    "_VersionHistory_F5F8918F-0A3F-4DA9-AE71-184EE5012880",
                    "Body",
                    MODIFIED_COL_NAME,
                    "Test_VersionHistory_F5F8918F-0A3F-4DA9-AE71-184EE5012880",
                ]
            );
            assert_eq!(flat.columns.last().unwrap().data_type, DataType::Long);
            assert!(flat.columns.last().unwrap().auto_number);
            assert_eq!(flat.indexes.len(), 2);
            assert_eq!(flat.indexes[0].name, FLAT_PK_INDEX_NAME);
        }

        // the memo column and the table carry the AppendOnly property
        {
            let mut maps = db.get_table_properties(test_page).expect("properties");
            let table_append_only = maps
                .get_default()
                .get("AppendOnly")
                .map(|prop| prop.value.clone());
            assert_eq!(
                table_append_only,
                Some(Value::Bool(true)),
                "table AppendOnly property"
            );
            let column_append_only = maps
                .get_mut("Body")
                .get("AppendOnly")
                .map(|prop| prop.value.clone());
            assert_eq!(
                column_append_only,
                Some(Value::Bool(true)),
                "memo column AppendOnly property"
            );
        }

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `add_version` / `versions` round trip through the flat table, also after
    /// reopening the database, and sort the versions newest first.  The
    /// strongly typed update/delete methods are refused like jackcess does.
    #[test]
    fn version_history_api_round_trips() {
        let path = test_tmp_dir().join(format!(
            "jackcess_complex_vh_api_{}.accdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        db.create_table(&version_history_spec())
            .expect("create table");
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Text("one".to_string()),
                Value::Text("initial body".to_string()),
            ]],
        )
        .expect("add row");
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();
        {
            let mut fk = db
                .version_history_fk("Test", row_id, "Body")
                .expect("version history fk");
            assert_eq!(fk.versioned_column_name(), Some("Body"));
            assert!(fk.versions().expect("versions").is_empty());
            let v1 = fk
                .add_version("v1 body", LocalDateTime::new(2021, 1, 2, 3, 4, 5, 0))
                .expect("add v1");
            let v2 = fk
                .add_version("v2 body", LocalDateTime::new(2021, 2, 3, 4, 5, 6, 0))
                .expect("add v2");
            let v3 = fk
                .add_version("v3 body", LocalDateTime::new(2021, 3, 4, 5, 6, 7, 0))
                .expect("add v3");
            assert_eq!((v1.id, v2.id, v3.id), (1, 2, 3));
            let versions = fk.versions().expect("versions");
            assert_eq!(versions.len(), 3);
            assert_eq!(versions[0].id, 3, "newest first");
            assert_eq!(versions[2].id, 1);
            assert_eq!(versions[0].value.as_deref(), Some("v3 body"));
            // the date is stored as an Access date double, so compare the
            // stored instant rather than the rounded local date/time
            assert_eq!(
                crate::datenums::to_date_double(versions[0].modified_date.expect("modified date")),
                crate::datenums::to_date_double(LocalDateTime::new(2021, 3, 4, 5, 6, 7, 0))
            );
            // the strongly typed update/delete methods are not supported
            let mut updated = versions[0].clone();
            updated.value = Some("changed".to_string());
            assert!(fk.update_version(&updated).is_err());
            assert!(fk.delete_version(&versions[1]).is_err());
            assert!(fk.delete_all_values().is_err());
        }
        db.flush().expect("flush");
        drop(db);

        let mut db = Database::open_path(&path).expect("reopen");
        let info = db
            .version_history_info("Test", "Body")
            .expect("info")
            .expect("info present");
        assert_eq!(info.kind, ComplexDataType::VersionHistory);
        let (row_id, _) = db.read_rows_with_ids("Test").expect("rows")[0].clone();
        let versions = db
            .version_history_fk("Test", row_id, "Body")
            .expect("fk")
            .versions()
            .expect("versions");
        assert_eq!(versions.len(), 3);
        assert_eq!(versions[0].value.as_deref(), Some("v3 body"));
        assert_eq!(versions[2].value.as_deref(), Some("v1 body"));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A table can only keep one version history (every version history column
    /// has the same name), and only a memo column can keep one.
    #[test]
    fn version_history_column_declarations_are_validated() {
        let path = test_tmp_dir().join(format!(
            "jackcess_complex_vh_valid_{}.accdb",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);

        let two_histories = TableSpec::new(
            "Two",
            vec![
                ColumnSpec::new("A", DataType::Memo).set_append_only(true),
                ColumnSpec::new("B", DataType::Memo).set_append_only(true),
            ],
        );
        assert!(db.create_table(&two_histories).is_err());

        let text_history = TableSpec::new(
            "NotMemo",
            vec![ColumnSpec::new("A", DataType::Text).set_append_only(true)],
        );
        assert!(db.create_table(&text_history).is_err());

        // the same rules apply when a column is added to an existing table
        let plain = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        );
        db.create_table(&plain).expect("create plain table");
        db.add_rows("T", &[vec![Value::Null, Value::Text("a".to_string())]])
            .expect("add row");
        db.add_column(
            "T",
            ColumnSpec::new("Body", DataType::Memo).set_append_only(true),
        )
        .expect("add append-only memo column");
        assert!(db.is_append_only("T", "Body").expect("append only"));
        assert!(db
            .add_column(
                "T",
                ColumnSpec::new("Body2", DataType::Memo).set_append_only(true)
            )
            .is_err());
        assert!(db
            .add_column(
                "T",
                ColumnSpec::new("Text2", DataType::Text).set_append_only(true)
            )
            .is_err());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
