//! Port of `com.healthmarketscience.jackcess.impl.TableImpl`.
//!
//! This module also hosts the port of `com.healthmarketscience.jackcess.util`
//! `ErrorHandler`: the [`ErrorHandler`] trait and its
//! [`DefaultErrorHandler`], [`ReplacementErrorHandler`] and
//! [`DebugErrorHandler`] implementations, plus the [`ErrorLocation`] context
//! (`TableImpl.RowState`) passed to a handler.  A handler is consulted for
//! every column decode failure; it either returns a replacement value for that
//! column (the row is still returned) or propagates an error.

use std::sync::{Arc, Mutex};

use crate::bytebuf::ByteBuf;
use crate::column::{self, Column, ColumnSpec, ValidationRule};
use crate::datatype::DataType;
use crate::datenums::LocalDateTime;
use crate::error::{Error, Result};
use crate::format::JetFormat;
use crate::index::{IndexData, IndexImpl};
use crate::page_types;
use crate::pagechannel::PageChannel;
use crate::usagemap::UsageMap;
use crate::value::Value;

pub const MAGIC_TABLE_NUMBER: i32 = 1625;
pub const TYPE_SYSTEM: u8 = 0x53;
pub const TYPE_USER: u8 = 0x4e;
const OFFSET_MASK: i16 = 0x1FFF;
const DELETED_ROW_MASK: i16 = -32768i16;
const OVERFLOW_ROW_MASK: i16 = 0x4000;
const MAX_BYTE: usize = 256;

/// A row identifier: (page number, row number).
pub type RowId = (i32, i32);

// ---------------------------------------------------------------------------
// error handling (`com.healthmarketscience.jackcess.util.ErrorHandler`)
// ---------------------------------------------------------------------------

/// Port of `com.healthmarketscience.jackcess.util.ErrorHandler`: handler for
/// errors encountered while reading a column of row data from a [`Table`].
///
/// An instance may be configured at the [`Database`](crate::database::Database),
/// [`Table`] or [`Cursor`](crate::cursor::Cursor) level (a cursor handler wins
/// over the table handler, which wins over the database handler).  The default
/// instance is [`DEFAULT_ERROR_HANDLER`] ([`DefaultErrorHandler`]), which
/// rethrows the error.
///
/// Java's handler signature is
/// `Object handleRowError(Column, byte[], Location, Exception)`: the handler
/// may either throw (which the Rust trait models by returning `Err`) or return
/// a replacement value for this row's column, in which case the row continues
/// to be read normally.  Note that Jackcess never *skips* a row: the row is
/// returned with the replacement value (this is what `DebugErrorHandler`'s
/// `null` replacement produces, i.e. a row with a `null` in the broken column).
pub trait ErrorHandler {
    /// `ErrorHandler.handleRowError`.
    ///
    /// `column_data` is the raw on-page data of the column, which is `None`
    /// when the error was thrown before the bytes could be obtained (Java's
    /// `columnData` is `null` in that case).  `error` is passed by value so the
    /// default handler can rethrow the original error unchanged.
    fn handle_row_error(
        &self,
        column: &Column,
        column_data: Option<&[u8]>,
        location: &dyn ErrorLocation,
        error: Error,
    ) -> Result<Value>;
}

/// Port of `ErrorHandler.Location`: details about the row which failed
/// ("the table in which the error occurred" plus the `toString` context Java's
/// `TableImpl.RowState` provides).
pub trait ErrorLocation: std::fmt::Display {
    /// `ErrorHandler.Location.getTable`.
    fn get_table(&self) -> &Table;
}

/// The `ErrorHandler.Location` of a row read: the port of `TableImpl.RowState`
/// as far as the error handler context is concerned (the header row id and the
/// id of the row which actually holds the data after following overflow
/// pointers).
pub struct RowStateLocation<'a> {
    table: &'a Table,
    header_row_id: RowId,
    final_row_id: RowId,
}

impl<'a> RowStateLocation<'a> {
    pub fn new(
        table: &'a Table,
        header_row_id: RowId,
        final_row_id: RowId,
    ) -> RowStateLocation<'a> {
        RowStateLocation {
            table,
            header_row_id,
            final_row_id,
        }
    }

    /// The row id of the header row (`RowState.getHeaderRowId`).
    pub fn get_header_row_id(&self) -> RowId {
        self.header_row_id
    }

    /// The row id of the row data (`RowState.getFinalRowId`).
    pub fn get_final_row_id(&self) -> RowId {
        self.final_row_id
    }
}

impl std::fmt::Display for RowStateLocation<'_> {
    /// `RowState.toString` (`ToStringBuilder.valueBuilder` with the two row
    /// ids); `RowIdImpl.toString` is `page:row`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RowState[headerRowId={}:{},finalRowId={}:{}]",
            self.header_row_id.0, self.header_row_id.1, self.final_row_id.0, self.final_row_id.1
        )
    }
}

impl ErrorLocation for RowStateLocation<'_> {
    fn get_table(&self) -> &Table {
        self.table
    }
}

/// Port of `ErrorHandler.DEFAULT`: rethrows the error unchanged.
pub struct DefaultErrorHandler;

impl ErrorHandler for DefaultErrorHandler {
    fn handle_row_error(
        &self,
        _column: &Column,
        _column_data: Option<&[u8]>,
        _location: &dyn ErrorLocation,
        error: Error,
    ) -> Result<Value> {
        Err(error)
    }
}

/// The static `ErrorHandler.DEFAULT` instance.
pub static DEFAULT_ERROR_HANDLER: DefaultErrorHandler = DefaultErrorHandler;

/// Port of `com.healthmarketscience.jackcess.util.ReplacementErrorHandler`:
/// always returns the configured replacement value.
pub struct ReplacementErrorHandler {
    replacement: Value,
}

impl ReplacementErrorHandler {
    /// Constructs a handler which replaces errored values with `null`.
    pub fn new() -> ReplacementErrorHandler {
        ReplacementErrorHandler {
            replacement: Value::Null,
        }
    }

    /// Constructs a handler which replaces errored values with the given value.
    pub fn with_replacement(replacement: Value) -> ReplacementErrorHandler {
        ReplacementErrorHandler { replacement }
    }

    /// The replacement value returned by the handler.
    pub fn get_replacement(&self) -> &Value {
        &self.replacement
    }
}

impl Default for ReplacementErrorHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorHandler for ReplacementErrorHandler {
    fn handle_row_error(
        &self,
        _column: &Column,
        _column_data: Option<&[u8]>,
        _location: &dyn ErrorLocation,
        _error: Error,
    ) -> Result<Value> {
        Ok(self.replacement.clone())
    }
}

/// Port of `com.healthmarketscience.jackcess.util.DebugErrorHandler`: records a
/// debug entry for the failed column (the message is Java's
/// `"Failed reading column " + column + ", row " + location + ", bytes " +
/// (hex or "null")`) and then returns the configured replacement value
/// (`null` by default).
///
/// Java writes the entry through `System.Logger` at `DEBUG` level, which is
/// normally invisible; there is no logging framework in the Rust port, so the
/// modeled text output is collected in [`DebugErrorHandler::messages`] instead
/// (the output can also be obtained for a one-off error with
/// [`DebugErrorHandler::format_message`]).
///
/// The column rendering mirrors `ColumnImpl.toString()` (the `Column[...]`
/// fields) but necessarily omits Java's `@identityHashCode` and the lazily
/// loaded `validator`/`defaultValue` entries: those depend on a live Java
/// object graph and are not part of the portable port.  `SortOrder` is rendered
/// as its numeric value because the port has no sort-order name table.
pub struct DebugErrorHandler {
    replacement: Value,
    messages: Mutex<Vec<String>>,
}

impl DebugErrorHandler {
    /// Constructs a `DebugErrorHandler` which replaces all errored values with
    /// `null`.
    pub fn new() -> DebugErrorHandler {
        DebugErrorHandler {
            replacement: Value::Null,
            messages: Mutex::new(Vec::new()),
        }
    }

    /// Constructs a `DebugErrorHandler` which replaces all errored values with
    /// the given value.
    pub fn with_replacement(replacement: Value) -> DebugErrorHandler {
        DebugErrorHandler {
            replacement,
            messages: Mutex::new(Vec::new()),
        }
    }

    /// The debug entries recorded so far (the modeled `System.Logger` output).
    pub fn messages(&self) -> Vec<String> {
        self.messages.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Clears the recorded debug entries.
    pub fn clear_messages(&self) {
        if let Ok(mut m) = self.messages.lock() {
            m.clear();
        }
    }

    /// The text Java's `DebugErrorHandler.handleRowError` logs for the given
    /// error (`"Failed reading column " + column + ", row " + location +
    /// ", bytes " + (hex or "null")`).
    pub fn format_message(
        column: &Column,
        column_data: Option<&[u8]>,
        location: &dyn ErrorLocation,
    ) -> String {
        format!(
            "Failed reading column {}, row {}, bytes {}",
            format_column_display(column, &location.get_table().name),
            location,
            match column_data {
                Some(data) => format_hex_bytes(data),
                None => "null".to_string(),
            }
        )
    }
}

impl Default for DebugErrorHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl ErrorHandler for DebugErrorHandler {
    fn handle_row_error(
        &self,
        column: &Column,
        column_data: Option<&[u8]>,
        location: &dyn ErrorLocation,
        _error: Error,
    ) -> Result<Value> {
        if let Ok(mut messages) = self.messages.lock() {
            messages.push(Self::format_message(column, column_data, location));
        }
        Ok(self.replacement.clone())
    }
}

/// `ByteUtil.toHexString(byte[])`: lowercase hex with a space between bytes, an
/// extra space after every 4 bytes (`NUM_BYTES_PER_BLOCK`) and a newline after
/// every 24 bytes (`NUM_BYTES_PER_LINE`).
pub fn format_hex_bytes(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len() * 2 + data.len());
    for (i, byte) in data.iter().enumerate() {
        out.push_str(&format!("{:02x}", byte));
        let next = i + 1;
        if next < data.len() {
            if next % 24 == 0 {
                out.push('\n');
            } else {
                out.push(' ');
                if next % 4 == 0 {
                    out.push(' ');
                }
            }
        }
    }
    out
}

/// A deterministic model of `ColumnImpl.toString()` (without Java's identity
/// hash) used by [`DebugErrorHandler`]'s text output.
pub fn format_column_display(column: &Column, table_name: &str) -> String {
    let mut out = String::new();
    out.push_str("Column[");
    out.push_str(&format!("name: ({}) {} ", table_name, column.name));
    out.push_str(&format!(
        "type: 0x{:x} ({}) ",
        column.data_type.value(),
        column.data_type.get_name()
    ));
    out.push_str(&format!("number: {} ", column.column_number));
    out.push_str(&format!("id: {} ", column.column_id));
    out.push_str(&format!("length: {} ", column.column_length));
    out.push_str(&format!("variableLength: {}", column.variable_length));
    if column.hidden {
        out.push_str(&format!(" hidden: {}", column.hidden));
    }
    if column.security_identifier {
        out.push_str(&format!(
            " securityIdentifier: {}",
            column.security_identifier
        ));
    }
    if column.calculated {
        out.push_str(&format!(" calculated: {}", column.calculated));
    }
    if column.data_type.is_textual() {
        out.push_str(&format!(
            " compressedUnicode: {}",
            column.compressed_unicode
        ));
        if let Some(sort_order) = column.sort_order {
            out.push_str(&format!(
                " textSortOrder: SortOrder[0x{:08x}]",
                sort_order.value as u32
            ));
        }
        if column.code_page > 0 {
            out.push_str(&format!(" textCodePage: {}", column.code_page));
        }
        if column.append_only {
            out.push_str(&format!(" appendOnly: {}", column.append_only));
        }
    }
    if column.data_type.has_scale_precision() {
        out.push_str(&format!(
            " precision: {} scale: {}",
            column.precision, column.scale
        ));
    }
    // Java also appends `lastAutoNumber: N` for auto number columns; the last
    // value belongs to a live `AutoNumberGenerator` on the Java column, which
    // the port does not keep on the column definition.
    out.push(']');
    out
}

/// The resolved position of an existing row, the port of the state
/// `TableImpl.RowState` tracks while reading, updating and deleting a row.
///
/// The header row is the row the given `RowId` points at.  When that row has
/// been relocated to another page it holds only an overflow pointer, so the
/// row data is found in `final_page`, which may differ from the header page.
pub struct RowLocation {
    pub header_page_number: i32,
    pub header_row_number: i32,
    /// page buffer holding the header row
    pub header_page: ByteBuf,
    /// page number of the row data (the header page number unless the row was
    /// relocated)
    pub final_page_number: i32,
    /// row number of the row data
    pub final_row_number: i32,
    /// page buffer holding the row data
    pub final_page: ByteBuf,
    /// start offset of the row data within `final_page`
    pub row_start: usize,
    /// end offset of the row data within `final_page`
    pub row_end: usize,
    pub deleted: bool,
}

/// `TableImpl.IndexFeature`: how strictly
/// [`Table::find_index_for_columns`] must match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexFeature {
    /// only an exact match, regardless of uniqueness
    ExactOnly,
    /// only an exact match on a unique (or primary key) index
    ExactUniqueOnly,
    /// any index which begins with the given columns (the shortest one wins)
    AnyMatch,
}

/// A calculated column of a table (`TableImpl.CalcColEvaluator`'s list plus the
/// `CalcColEvalContext` of the column).
#[derive(Clone)]
pub struct CalcColumn {
    /// index of the column in [`Table::columns`]
    pub column_index: usize,
    /// the parsed `Expression` property (a parse failure is reported when the
    /// column is calculated, like the Java library reports it on evaluation)
    pub expression: std::result::Result<crate::expr::Expression, crate::expr::ExprError>,
    /// the calculated column's result type (`ResultType` property)
    pub result_type: DataType,
}

/// The read-only metadata of one column (`Column`, plus the values which need
/// the owning table's format to be computed, like `getLengthInUnits()`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnMetaData {
    /// `Column.getName()`.
    pub name: String,
    /// `Column.getType()`.
    pub data_type: DataType,
    /// `Column.getSQLType()`, `None` where Java throws `JackcessException`
    /// ("Unsupported data type", e.g. `GUID`).
    pub sql_type: Option<i32>,
    /// `Column.getLength()`: the raw length from the table definition.
    pub length: i16,
    /// `Column.getLengthInUnits()`: the length in characters/units.
    pub length_in_units: i32,
    /// `Column.getPrecision()`.
    pub precision: u8,
    /// `Column.getScale()`.
    pub scale: u8,
    /// `Column.isAutoNumber()`.
    pub auto_number: bool,
    /// `Column.isHidden()`.
    pub hidden: bool,
    /// `Column.isCalculated()`.
    pub calculated: bool,
    /// `Column.isAppendOnly()` as far as the table definition can tell: the
    /// raw `VERSION_HISTORY_EXT_FLAG_MASK` flag of a memo column.  A table has
    /// no database handle, so the authoritative resolution through the
    /// `MSysComplexColumns` link is done by
    /// [`crate::database::Database::get_column_meta_data_all`], which sets
    /// [`Self::version_history_column`] and corrects this flag.
    pub append_only: bool,
    /// `Column.getVersionHistoryColumn() != null`: the name of the version
    /// history complex column which tracks this memo column.  Only the
    /// database level metadata can resolve the link through
    /// `MSysComplexColumns`, so [`Table::get_column_meta_data_all`] reports
    /// `None` and the flag above is the raw definition flag.
    pub version_history_column: Option<String>,
    /// `Column.isVariableLength()`.
    pub variable_length: bool,
}

/// The read-only metadata of one index (the values of `Index` which need the
/// owning table's index data to be computed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexMetaData {
    /// `Index.getName()`.
    pub name: String,
    /// `Index.isPrimaryKey()`.
    pub primary_key: bool,
    /// `Index.isForeignKey()`.
    pub foreign_key: bool,
    /// `Index.isUnique()`.
    pub unique: bool,
    /// `Index.isRequired()`.
    pub required: bool,
    /// `Index.shouldIgnoreNulls()`.
    pub ignore_nulls: bool,
    /// `Index.getColumns()`: the column names, in index order.
    pub columns: Vec<String>,
    /// `Index.Column.isAscending()` for every entry of [`Self::columns`].
    pub ascending: Vec<bool>,
}

pub struct Table {
    pub name: String,
    pub table_def_page_number: i32,
    pub flags: i32,
    pub row_count: i32,
    pub last_long_auto_number: i32,
    pub last_complex_auto_number: i32,
    pub table_type: u8,
    pub max_column_count: i16,
    pub max_var_column_count: i16,
    pub columns: Vec<Column>,
    /// indices into `columns`, ordered by var len table index
    pub var_columns: Vec<usize>,
    pub auto_num_columns: Vec<usize>,
    pub index_datas: Vec<IndexData>,
    pub indexes: Vec<IndexImpl>,
    pub owned_pages: Option<UsageMap>,
    pub free_space_pages: Option<UsageMap>,
    pub index_count: i32,
    pub logical_index_count: i32,
    pub mod_count: i32,
    pub write_def_error: Option<String>,
    /// long value column usage maps, keyed by column number
    pub column_umaps: std::collections::HashMap<i16, (UsageMap, UsageMap)>,
    /// persistent row write buffer (mirrors TempBufferHolder soft reference)
    pub write_row_buffer: Option<ByteBuf>,
    /// `TableImpl._allowAutoNumInsert`: a per table override of the database's
    /// `allowAutoNumberInsert` policy (`None` means "use the database policy")
    pub allow_auto_number_insert: Option<bool>,
    /// `TableImpl._nameMap`: the raw value of the `NameMap` property, captured
    /// when the table's property maps are loaded (see
    /// [`Table::apply_property_maps`]) and parsed on demand by
    /// [`Table::get_name_map`].  Java's `getNameMap` parses the property the
    /// same way (lazily, from `getProperties()`), so malformed data is reported
    /// at that call rather than when the table is opened.
    pub name_map_blob: Option<Vec<u8>>,
    /// The owning database's `DateTimeType` (`TableImpl.getDatabase()
    /// .getDateTimeType()`, which every column's `DateTimeFactory` asks for).
    /// Java reads it from the database on every date conversion; the port has
    /// no back reference from a table to its database, so
    /// [`crate::database::Database::set_date_time_type`] copies the new value
    /// into every loaded table (and a table loaded later takes the current
    /// value), which is observationally identical through the database API.
    pub date_time_type: crate::database::DateTimeType,
    /// The owning database's zone (`TableImpl.getDatabase().getTimeZone()`),
    /// copied in by [`crate::database::Database::apply_table_settings`] for the
    /// same reason as [`Table::date_time_type`].  The property driven
    /// expressions (calculated columns, default values and validation rules)
    /// need it: Java's `BaseEvalContext.toValue` converts a `DATE`-mode
    /// `DateExt` to a `LocalDateTime` through
    /// `LocalDateTime.ofInstant(getTime(), getZoneId())`.
    pub time_zone: crate::datenums::TimeZone,
    /// `TableImpl`'s column list order: `Table.ColumnOrder.DATA` (the order of
    /// the data in the table definition) or `DISPLAY` (sorted by
    /// `ColumnImpl.getDisplayIndex()`).  Java applies the database's
    /// `Table.ColumnOrder` when the table is *constructed* (`TableImpl` sorts
    /// its `_columns` list once), so changing `Database.setColumnOrder`
    /// afterwards does not re-order an already loaded table; the port keeps the
    /// same one-shot semantics in [`Table::get_columns`].
    ///
    /// jackcess 5.0.4 assigns the display index from the order the column
    /// definitions are read in, so the two orders coincide for every table; the
    /// `ColumnOrder` column property does not feed the display index.
    pub column_order: crate::database::ColumnOrder,
    /// the simple file name of the owning database (`DatabaseImpl.getName`),
    /// used in the column validator error messages
    pub db_name: String,
    /// the expression based column validation rules, one entry per
    /// [`Table::columns`] entry (`ColumnImpl._validator`'s
    /// `ColValidatorEvalContext`).  Only present when the owning database
    /// evaluates expressions, exactly like `ColumnImpl
    /// .reloadPropertiesValidators`.
    pub column_validators: Vec<Option<ValidationRule>>,
    /// the expression based table row validation rule (`TableImpl
    /// ._rowValidator`), only present when the owning database evaluates
    /// expressions (`TableImpl.reloadRowValidator`).
    pub row_validator: Option<ValidationRule>,
    /// the calculated columns of this table, ordered by calculation dependency
    /// (`TableImpl.CalcColEvaluator._calcColumns`)
    pub calc_columns: Vec<CalcColumn>,
    /// whether [`Table::calc_columns`] is in dependency order
    pub calc_sorted: bool,
    /// `true` for a type 4 (linked ODBC) catalog object, whose data is owned by
    /// the remote database: the port of `TableDefinitionImpl`, which supports
    /// the metadata APIs but no row access.  A [`Table`] is never
    /// definition-only for any other catalog type.
    pub definition_only: bool,
    /// `Table.getCreatedDate()`: the catalog `DateCreate` value, filled in when
    /// the table is loaded from the database (Java reads it on demand).
    pub created_date: Option<LocalDateTime>,
    /// `Table.getUpdatedDate()`: the catalog `DateUpdate` value.
    pub updated_date: Option<LocalDateTime>,
    /// the format of the database which owns this table, kept so that the
    /// metadata helpers can convert lengths to units
    pub format: JetFormat,
    /// table level [`ErrorHandler`] (`TableImpl._tableErrorHandler`); when
    /// `None` the database level handler is used
    pub error_handler: Option<Arc<dyn ErrorHandler>>,
    /// the owning database's [`ErrorHandler`], copied into the table when it is
    /// loaded or changed (`TableImpl.getErrorHandler` asks the database
    /// directly)
    pub db_error_handler: Option<Arc<dyn ErrorHandler>>,
}

/// The cached value of one row slot (`TableImpl.RowState._rowValues`).
///
/// Java's cache holds arbitrary `Object`s and `getRowCacheValue` returns only
/// the immutable ones (`byte[]` is the only mutable value a read produces).
/// The raw bytes of a column which failed to read are cached as a `RawData`
/// wrapper, which counts as immutable; the port has no separate wrapper type,
/// so the two cases are distinguished here.
#[derive(Clone, Debug)]
enum CacheEntry {
    /// a value `ColumnImpl.read` returned
    Value(Value),
    /// `ColumnImpl.rawDataWrapper(columnData)`: the bytes of a column which
    /// could not be read (the cache returns them like Java's `RawData`)
    Raw(Vec<u8>),
}

/// The read side of `TableImpl.RowState`: the lazily read null mask, the row
/// buffer mark state, the cached Jet 3 jump table and the per column value
/// cache.
///
/// Java reuses one `RowState` per cursor; the port creates a reader for each
/// row read (`getRow`/`getRowColumn`), which is the state a freshly positioned
/// cursor has.  Keeping the state local matters for the corrupt null mask
/// paths: the mask read is retried per column, and `getRowColumnData` fails for
/// every non null, non boolean column once a corrupt mask discarded the mark.
struct RowReader<'a> {
    row_buffer: &'a ByteBuf,
    /// absolute page offset of the row start: `narrowToRow`'s mark
    row_start: usize,
    /// `RowState._nullMask`, set only after a successful read
    null_mask: Option<NullMask>,
    /// whether the mark set at the row start is still valid (a
    /// `Buffer.position(int)` in front of it discards it)
    mark_valid: bool,
    /// `RowState._varColOffsets` (the Jet 3 jump table, read once per row)
    var_col_offsets: Option<Vec<i32>>,
    /// `RowState._rowValues`
    cache: Vec<Option<CacheEntry>>,
    handler: Option<&'a dyn ErrorHandler>,
    location: Option<&'a dyn ErrorLocation>,
}

impl<'a> RowReader<'a> {
    fn new(
        row_buffer: &'a ByteBuf,
        column_count: usize,
        handler: Option<&'a dyn ErrorHandler>,
        location: Option<&'a dyn ErrorLocation>,
    ) -> RowReader<'a> {
        RowReader {
            row_start: row_buffer.position(),
            row_buffer,
            null_mask: None,
            mark_valid: true,
            var_col_offsets: None,
            cache: vec![None; column_count],
            handler,
            location,
        }
    }

    /// `RowState.getNullMask`: reads the mask on demand and caches it after a
    /// successful read (a failure leaves it unset, so the next column retries).
    ///
    /// Like `getRowNullMask`, the read starts with the `rowBuffer.reset()` the
    /// `narrowToRow` mark makes possible; a mark already discarded by an
    /// earlier `position(...)` fails here with an `InvalidMarkException`.
    fn null_mask(&mut self, format: &JetFormat) -> Result<&NullMask> {
        if self.null_mask.is_none() {
            if !self.mark_valid {
                return Err(Error::InvalidMark);
            }
            let (mask, position) = Table::row_null_mask(self.row_buffer, format)?;
            // `Buffer.position(newPosition)` discards the mark when the new
            // position lies in front of it
            self.mark_valid = position >= self.row_start;
            self.null_mask = Some(mask);
        }
        Ok(self.null_mask.as_ref().expect("null mask was just read"))
    }

    /// `RowState.getRowCacheValue`: only immutable values are returned (a
    /// `byte[]` could have been modified by the caller).
    fn cache_value(&self, column_index: usize) -> Option<Value> {
        match self.cache.get(column_index).and_then(Option::as_ref) {
            Some(CacheEntry::Value(Value::Bytes(_))) => None,
            Some(CacheEntry::Value(value)) => Some(value.clone()),
            Some(CacheEntry::Raw(bytes)) => Some(Value::Bytes(bytes.clone())),
            None => None,
        }
    }

    /// Whether the row buffer mark is still valid (see [`RowReader::mark_valid`]).
    fn mark_valid(&self) -> bool {
        self.mark_valid
    }

    /// `RowState.getRowCacheValues`: the whole cache, nulls included.
    fn cache_values(&self) -> Vec<Value> {
        self.cache
            .iter()
            .map(|entry| match entry {
                Some(CacheEntry::Value(value)) => value.clone(),
                Some(CacheEntry::Raw(bytes)) => Value::Bytes(bytes.clone()),
                None => Value::Null,
            })
            .collect()
    }

    /// `TableImpl.getRawRowValue`: the raw on-page bytes of the given column
    /// without making a value of them (which for a long value column is the
    /// definition of the value rather than the value itself), or `None` when
    /// the column is null in the row.
    ///
    /// Unlike [`RowReader::get_row_column`] this does not consult the row cache
    /// and reports every failure to the caller; Java's `readRawRowValue` is the
    /// wrapper which swallows them.
    fn get_raw_row_value(
        &mut self,
        table: &Table,
        ch: &mut PageChannel,
        column_index: usize,
    ) -> Result<Option<Vec<u8>>> {
        let col = &table.columns[column_index];
        let is_null = self.null_mask(&ch.format)?.is_null(col.column_number);
        if is_null {
            return Ok(None);
        }
        if col.is_variable_length()
            && ch.format.size_row_var_col_offset != 2
            && self.var_col_offsets.is_none()
        {
            let null_mask = self.null_mask.as_ref().expect("null mask was read");
            self.var_col_offsets = Some(read_jump_table_var_col_offsets(
                self.row_buffer,
                self.row_start,
                null_mask,
            )?);
        }
        Ok(Some(table.get_row_column_data(
            ch,
            self.row_buffer,
            col,
            self.null_mask.as_ref().expect("null mask was read"),
            self.var_col_offsets.as_deref(),
            self.mark_valid,
        )?))
    }

    /// `TableImpl.getRowColumn`: one column of the row, with the lazy null mask,
    /// the row cache and the error handler callbacks of the Java method.
    ///
    /// The mask is read first, even when the value is already cached, and the
    /// boolean/null early returns do not touch the row bytes, exactly like the
    /// Java method.
    fn get_row_column(
        &mut self,
        table: &Table,
        ch: &mut PageChannel,
        column_index: usize,
    ) -> Result<Value> {
        let col = &table.columns[column_index];
        // the raw bytes of the column, captured when they could be obtained
        // (Java's `columnData`, passed to the error handler)
        let mut column_data: Option<Vec<u8>> = None;
        let computed: Result<Value> = (|| {
            let is_null = self.null_mask(&ch.format)?.is_null(col.column_number);
            if col.store_in_null_mask() {
                // boolean values are stored in the null mask (and cached)
                return Ok(col.read_from_null_mask(is_null));
            }
            if is_null {
                // a null value is not cached
                return Ok(Value::Null);
            }
            if let Some(value) = self.cache_value(column_index) {
                // we already have it, use it
                return Ok(value);
            }
            if col.is_variable_length()
                && ch.format.size_row_var_col_offset != 2
                && self.var_col_offsets.is_none()
            {
                let null_mask = self.null_mask.as_ref().expect("null mask was read");
                self.var_col_offsets = Some(read_jump_table_var_col_offsets(
                    self.row_buffer,
                    self.row_start,
                    null_mask,
                )?);
            }
            let data = table.get_row_column_data(
                ch,
                self.row_buffer,
                col,
                self.null_mask.as_ref().expect("null mask was read"),
                self.var_col_offsets.as_deref(),
                self.mark_valid,
            )?;
            column_data = Some(data.clone());
            table.read_column_value(ch, col, &data)
        })();
        match computed {
            Ok(value) => {
                if !value.is_null() {
                    self.cache[column_index] = Some(CacheEntry::Value(value.clone()));
                }
                Ok(value)
            }
            Err(error) => match (self.handler, self.location) {
                (Some(handler), Some(location)) => {
                    // `getRowColumn`'s catch block caches the raw bytes before
                    // handing the error to the handler (the replacement is
                    // returned to the caller, not cached)
                    self.cache[column_index] = column_data.clone().map(CacheEntry::Raw);
                    handler.handle_row_error(col, column_data.as_deref(), location, error)
                }
                _ => Err(error),
            },
        }
    }
}

impl Table {
    pub fn is_system(&self) -> bool {
        self.table_type != TYPE_USER
    }

    /// `true` for a type 4 (linked ODBC) table definition, whose rows live in
    /// the remote database (`Table.getType() == LINKED_ODBC` in Java terms).
    pub fn is_definition_only(&self) -> bool {
        self.definition_only
    }

    /// `Table.getErrorHandler`: the table level handler when one is installed,
    /// otherwise the database level handler, otherwise
    /// [`DEFAULT_ERROR_HANDLER`].  Always non-`None`, like Java.
    pub fn get_error_handler(&self) -> &dyn ErrorHandler {
        if let Some(handler) = &self.error_handler {
            return handler.as_ref();
        }
        if let Some(handler) = &self.db_error_handler {
            return handler.as_ref();
        }
        &DEFAULT_ERROR_HANDLER
    }

    /// `Table.setErrorHandler`: `None` resets to the database level handler.
    pub fn set_error_handler(&mut self, new_error_handler: Option<Arc<dyn ErrorHandler>>) {
        self.error_handler = new_error_handler;
    }

    /// The handler a row read should use: the table's own handler or the
    /// database's (as an owned reference so callers can hold it across a
    /// mutable borrow of the table).
    pub(crate) fn error_handler_override(&self) -> Option<Arc<dyn ErrorHandler>> {
        self.error_handler
            .clone()
            .or_else(|| self.db_error_handler.clone())
    }

    /// Updates the copied database level handler
    /// (`TableImpl.getErrorHandler`'s database fallback).
    pub(crate) fn set_database_error_handler(&mut self, handler: Option<Arc<dyn ErrorHandler>>) {
        self.db_error_handler = handler;
    }

    /// `TableImpl.withErrorContext`.
    fn with_error_context(&self, msg: impl std::fmt::Display) -> String {
        format!("{} (Db={};Table={})", msg, self.db_name, self.name)
    }

    /// `TableDefinitionImpl`: every row traversal or write goes through
    /// `RowState`/`addRows` in Java, both of which throw
    /// `UnsupportedOperationException("TableDefinition has no data access")`.
    pub fn check_data_access(&self) -> Result<()> {
        if self.definition_only {
            return Err(Error::Unsupported(
                self.with_error_context("TableDefinition has no data access"),
            ));
        }
        Ok(())
    }

    /// `Table.getRowCount()`.
    pub fn get_row_count(&self) -> i32 {
        self.row_count
    }

    /// `Table.getCreatedDate()`: the catalog `DateCreate` value, filled in when
    /// the table is loaded from the database (Java reads it on demand).  The
    /// system catalog keeps its own date too (`MSysObjects` has a catalog row
    /// like every other table).
    pub fn get_created_date(&self) -> Option<LocalDateTime> {
        self.created_date
    }

    /// `Table.getUpdatedDate()`: the catalog `DateUpdate` value.
    pub fn get_updated_date(&self) -> Option<LocalDateTime> {
        self.updated_date
    }

    /// `Table.getName()`.
    pub fn get_name(&self) -> &str {
        &self.name
    }

    /// `Table.isHidden()`: whether the catalog flags mark a hidden object
    /// (like Java, a hidden table is still a normal, non-system table).
    pub fn is_hidden(&self) -> bool {
        (self.flags & crate::database::HIDDEN_OBJECT_FLAG) != 0
    }

    /// `Table.getColumnCount()`.
    pub fn get_column_count(&self) -> usize {
        self.columns.len()
    }

    /// `TableImpl.getMaxColumnCount()`: the maximum number of columns the table
    /// definition reserves slots for (not the number of defined columns).
    pub fn get_max_column_count(&self) -> i32 {
        self.max_column_count as i32
    }

    /// `TableImpl.hasColumn(String)`: a case-insensitive column name lookup
    /// which does not throw for an unknown name.
    pub fn has_column(&self, name: &str) -> bool {
        self.columns
            .iter()
            .any(|column| column.name.eq_ignore_ascii_case(name))
    }

    /// `Table.getIndexes()`: every index of this table, in index order.
    pub fn get_indexes(&self) -> &[IndexImpl] {
        &self.indexes
    }

    /// `TableImpl.getIndexDatas()`: the physical index data blocks (which can
    /// cover several logical indexes), in slot order.
    pub fn get_index_datas(&self) -> &[IndexData] {
        &self.index_datas
    }

    /// `TableImpl.getLogicalIndexCount()`: the number of index slots, which may
    /// exceed the number of defined indexes.
    pub fn get_logical_index_count(&self) -> i32 {
        self.logical_index_count
    }

    /// `TableImpl.getTableDefPageNumber()`: the page of this table's table
    /// definition (the catalog `Id` of a local table).
    pub fn get_table_def_page_number(&self) -> i32 {
        self.table_def_page_number
    }

    /// `TableImpl.getApproximateOwnedPageCount()`: the approximate number of
    /// database pages owned by this table and its indexes.  It does not count
    /// the pages of large OLE/MEMO values beyond their long-value usage maps.
    ///
    /// The formula is Java's: one page for the table definition, the table's
    /// owned pages, the owned pages of every long-value column and of every
    /// physical index.  A table definition without data access (a linked ODBC
    /// table) has no such count.
    pub fn get_approximate_owned_page_count(&self) -> i32 {
        // `add a page for the table def (although that might actually be more
        // than one page)`
        let mut count = self
            .owned_pages
            .as_ref()
            .map(|map| map.page_count())
            .unwrap_or(0) as i32
            + 1;
        // `col.getOwnedPageCount()`: only long value columns own pages, and
        // their maps live in `column_umaps` (a map which could not be read at
        // load time is skipped, like Java's `setUsageMaps`).
        for (owned, _) in self.column_umaps.values() {
            count += owned.page_count() as i32;
        }
        // note, we count owned pages from physical indexes, not logical ones
        for index_data in &self.index_datas {
            count += index_data
                .owned_pages
                .as_ref()
                .map(|map| map.page_count())
                .unwrap_or(0) as i32;
        }
        count
    }

    /// `Table.getIndex(String)`: the index of this table with the given name
    /// (case-insensitive).  Java throws `IllegalArgumentException` for an
    /// unknown name; the port returns [`Error::IllegalArgument`].
    pub fn get_index(&self, name: &str) -> Result<&IndexImpl> {
        self.indexes
            .iter()
            .find(|index| index.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| {
                Error::IllegalArgument(self.with_error_context(format!(
                    "Index with name {} does not exist on this table",
                    name
                )))
            })
    }

    /// `Table.getPrimaryKeyIndex()`.  Java throws `IllegalArgumentException`
    /// when the table has no primary key.
    pub fn get_primary_key_index(&self) -> Result<&IndexImpl> {
        self.indexes
            .iter()
            .find(|index| index.is_primary_key())
            .ok_or_else(|| {
                Error::IllegalArgument(self.with_error_context("No primary key index found"))
            })
    }

    /// `Table.getForeignKeyIndex(Table otherTable)`: the foreign key index of
    /// this table which references the given table's definition page.  Java
    /// throws `IllegalArgumentException` when there is none.
    pub fn get_foreign_key_index(&self, other_table: &Table) -> Result<&IndexImpl> {
        self.indexes
            .iter()
            .find(|index| {
                index.is_foreign_key()
                    && index.reference.as_ref().is_some_and(|reference| {
                        reference.other_table_page_number == other_table.table_def_page_number
                    })
            })
            .ok_or_else(|| {
                Error::IllegalArgument(self.with_error_context(format!(
                    "No foreign key reference to {} found",
                    other_table.name
                )))
            })
    }

    /// `Column.getSQLType()` for one column (`None` where Java throws for a
    /// data type without a SQL type).
    pub fn column_sql_type(&self, column: &Column) -> Option<i32> {
        column.data_type.get_sql_type()
    }

    /// `Column.getLengthInUnits()` for one column.  A calculated `TEXT` column
    /// keeps its calculated-field overhead in the stored length
    /// (`CalcTextColImpl.calcLengthInUnits`), so that overhead is removed here.
    pub fn column_length_in_units(&self, column: &Column) -> i32 {
        self.column_length_in_units_with_format(column, &self.format)
    }

    fn column_length_in_units_with_format(&self, column: &Column, format: &JetFormat) -> i32 {
        if column.calculated && column.data_type == DataType::Text {
            column.calculated_text_units(format)
        } else {
            column.length_in_units(format)
        }
    }

    /// The [`ColumnMetaData`] of the column with the given name (matched case
    /// insensitively, like [`Table::get_column`]).
    pub fn get_column_meta_data(&self, name: &str) -> Option<ColumnMetaData> {
        self.columns
            .iter()
            .find(|column| column.name.eq_ignore_ascii_case(name))
            .map(|column| self.column_meta_data_with_format(column, &self.format))
    }

    /// The [`ColumnMetaData`] of every column, in table definition order
    /// (`Table.getColumns()`).
    pub fn get_column_meta_data_all(&self) -> Vec<ColumnMetaData> {
        self.columns
            .iter()
            .map(|column| self.column_meta_data_with_format(column, &self.format))
            .collect()
    }

    fn column_meta_data_with_format(&self, column: &Column, format: &JetFormat) -> ColumnMetaData {
        ColumnMetaData {
            name: column.name.clone(),
            data_type: column.data_type,
            sql_type: column.data_type.get_sql_type(),
            length: column.column_length,
            length_in_units: self.column_length_in_units_with_format(column, format),
            // `ColumnImpl.getPrecision`/`getScale` return the type defaults
            // unless the column keeps the values in its definition
            // (`NumericColumnImpl`, i.e. a data type which has scale/precision)
            precision: column.get_precision(),
            scale: column.get_scale(),
            auto_number: column.auto_number,
            hidden: column.hidden,
            calculated: column.calculated,
            append_only: column.append_only,
            // the version history link needs the database (see the field doc)
            version_history_column: None,
            variable_length: column.is_variable_length(),
        }
    }

    /// The [`IndexMetaData`] of the given index.
    pub fn index_meta_data(&self, index: &IndexImpl) -> IndexMetaData {
        let data = self.index_data_for(index);
        IndexMetaData {
            name: index.name.clone(),
            primary_key: index.is_primary_key(),
            foreign_key: index.is_foreign_key(),
            unique: data
                .map(|d| d.is_unique())
                .unwrap_or(index.is_primary_key()),
            required: data.map(|d| d.is_required()).unwrap_or(false),
            ignore_nulls: data.map(|d| d.should_ignore_nulls()).unwrap_or(false),
            columns: data
                .map(|d| d.columns.iter().map(|c| c.column.name.clone()).collect())
                .unwrap_or_default(),
            ascending: data
                .map(|d| d.columns.iter().map(|c| c.is_ascending()).collect())
                .unwrap_or_default(),
        }
    }

    pub fn get_column(&self, name: &str) -> Option<&Column> {
        self.columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// `ColumnImpl.getDisplayIndex()` of one of this table's columns,
    /// addressed by its data order index.
    pub fn column_display_index(&self, data_index: usize) -> Option<usize> {
        self.columns.get(data_index).map(|c| c.display_index)
    }

    /// `Table.getColumns()`: the table's columns, in the order selected by
    /// [`Table::get_column_order`] (`ColumnOrder.DATA` by default).
    ///
    /// This is a *view*: it only changes the iteration order.  Row value
    /// arrays, the column ids/numbers and every write stay in data order
    /// (indexed by [`Column::column_index`]), exactly like `TableImpl`, which
    /// sorts its `_columns` list but keeps `getRowValue(rowArray)` using
    /// `_columnIndex` and the fixed/var column offsets.
    pub fn get_columns(&self) -> Vec<&Column> {
        let mut columns: Vec<&Column> = self.columns.iter().collect();
        if self.column_order == crate::database::ColumnOrder::Display {
            // `Collections.sort(_columns, ColumnImpl.DISPLAY_ORDER_COMPARATOR)`;
            // the sort is stable, so equal display indexes keep data order
            columns.sort_by_key(|c| c.display_index);
        }
        columns
    }

    /// The columns in data order (the order of [`Table::columns`]).  This is
    /// the order of a row value array, whatever the configured
    /// [`Table::get_column_order`] is.
    pub fn get_columns_in_data_order(&self) -> Vec<&Column> {
        self.columns.iter().collect()
    }

    /// `TableImpl.getDatabase().getDateTimeType()`: the date/time
    /// representation this table reads and writes.
    pub fn get_date_time_type(&self) -> crate::database::DateTimeType {
        self.date_time_type
    }

    /// Copies the owning database's `DateTimeType` into the table
    /// (`TableImpl` reads it from the database on demand; see the
    /// [`Table::date_time_type`] field docs).
    pub fn set_date_time_type(&mut self, date_time_type: crate::database::DateTimeType) {
        self.date_time_type = date_time_type;
    }

    /// Copies the owning database's time zone into the table (see
    /// [`Table::time_zone`]).
    pub fn set_time_zone(&mut self, time_zone: crate::datenums::TimeZone) {
        self.time_zone = time_zone;
    }

    /// The database zone the table was opened with (`TableImpl.getDatabase()
    /// .getTimeZone()`).
    pub fn get_time_zone(&self) -> crate::datenums::TimeZone {
        self.time_zone
    }

    /// `Table.ColumnOrder` which [`Table::get_columns`] uses.
    pub fn get_column_order(&self) -> crate::database::ColumnOrder {
        self.column_order
    }

    /// Sets the [`Table::get_columns`] order.  Java only applies the database's
    /// order while constructing the table, so the database setter does not
    /// change an already loaded table; this is the per table equivalent of
    /// having opened it with that order.
    pub fn set_column_order(&mut self, column_order: crate::database::ColumnOrder) {
        self.column_order = column_order;
    }

    /// The raw value of the `NameMap` property captured with the table's
    /// property maps, if the property is present.
    pub fn get_name_map_blob(&self) -> Option<&[u8]> {
        self.name_map_blob.as_deref()
    }

    /// `Table.getNameMap()`: the name map stored in this table's `NameMap`
    /// property, or `None` if the table has none.
    ///
    /// The value is parsed on every call, which is indistinguishable from
    /// Java's cached instance for a read-only caller; a malformed blob reports
    /// the `NameMapImpl` `IOException` (as [`Error::Jackcess`]), exactly when
    /// Java reports it.
    pub fn get_name_map(&self) -> Result<Option<crate::namemap::TableNameMap>> {
        match self.name_map_blob.as_deref() {
            Some(bytes) => Ok(Some(crate::namemap::TableNameMap::read(bytes)?)),
            None => Ok(None),
        }
    }

    /// `Table.getNameMap()` for a caller which loaded the property maps itself.
    ///
    /// The database fills [`Table::name_map_blob`] for every table it loads
    /// (user tables through [`Table::apply_property_maps`], system tables and
    /// the system catalog through
    /// [`Database::get_table_name_map`](crate::database::Database::get_table_name_map)'s
    /// value), so this is only needed for maps the caller obtained elsewhere.
    pub fn get_name_map_from(
        &self,
        properties: &crate::property::PropertyMaps,
    ) -> Result<Option<crate::namemap::TableNameMap>> {
        let value = properties
            .default_map()
            .and_then(|map| map.get_value(crate::property::NAME_MAP_PROP));
        match value {
            Some(Value::Bytes(bytes)) => Ok(Some(crate::namemap::TableNameMap::read(bytes)?)),
            _ => Ok(None),
        }
    }

    /// The index data backing a logical index (`IndexImpl.getIndexData`).
    pub fn index_data_for(&self, idx: &IndexImpl) -> Option<&IndexData> {
        self.index_datas
            .iter()
            .find(|d| d.number == idx.index_data_number)
    }

    /// `TableImpl.findIndexForColumns`: the index of this table whose leading
    /// columns are exactly the given columns (matched case insensitively).
    pub fn find_index_for_columns(
        &self,
        search_columns: &[String],
        feature: IndexFeature,
    ) -> Option<&IndexImpl> {
        let mut partial: Option<&IndexImpl> = None;
        for index in &self.indexes {
            let data = match self.index_data_for(index) {
                Some(d) => d,
                None => continue,
            };
            let index_columns = &data.columns;
            if index_columns.len() < search_columns.len() {
                continue;
            }
            let exact_match = index_columns.len() == search_columns.len();
            let mut search_matches = true;
            for (search, icol) in search_columns.iter().zip(index_columns.iter()) {
                if !search.eq_ignore_ascii_case(&icol.column.name) {
                    search_matches = false;
                    break;
                }
            }
            if search_matches {
                if exact_match && (feature != IndexFeature::ExactUniqueOnly || data.is_unique()) {
                    return Some(index);
                }
                if !exact_match && feature == IndexFeature::AnyMatch {
                    let better = match partial {
                        None => true,
                        Some(p) => match self.index_data_for(p) {
                            Some(pd) => index_columns.len() < pd.columns.len(),
                            None => true,
                        },
                    };
                    if better {
                        partial = Some(index);
                    }
                }
            }
        }
        partial
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// Applies the loaded property maps to the columns, the port of
    /// `ColumnImpl.initColumnValidator` / `reloadPropertiesValidators`: the
    /// `DefaultValue`, `Required` and `AllowZeroLength` properties become the
    /// column's default value generator and internal validators.  Autonumber
    /// columns are left alone, exactly like the Java library (which skips the
    /// property based functionality for them).  A calculated column's
    /// `ResultType` property determines the "real" data type (the type stored
    /// in the table definition is only the one the builder declared) and its
    /// `Expression` property gives the expression the row values are computed
    /// with.
    ///
    /// `evaluate_expressions` is the owning database's
    /// [`crate::database::Database::is_evaluate_expressions`] policy.  Java
    /// applies it as follows (`ColumnImpl.reloadPropertiesValidators`):
    /// - a calculated column only gets its expression evaluator when the
    ///   policy is on (`CalcColEvalContext calcCol = null` otherwise), so no
    ///   value is computed for it and `TableImpl.CalcColEvaluator.add` ignores
    ///   it;
    /// - the `DefaultValue` is only parsed when the policy is on (the method
    ///   returns before the default value context is built);
    /// - `Required` and `AllowZeroLength` are applied *before* the policy
    ///   check and therefore still take effect when it is off;
    /// - the expression based column validation rule and the table row rule
    ///   are skipped when it is off.
    ///
    /// The rules are re-read on every call (`propertiesUpdated` reloads the
    /// validators as well), so a property update which changes `ValidationRule`
    /// / `ValidationText` takes effect on the next row operation.
    pub fn apply_property_maps(
        &mut self,
        maps: &crate::property::PropertyMaps,
        evaluate_expressions: bool,
    ) {
        // `TableImpl.getNameMap` reads the name map lazily from
        // `getProperties()`, which the database loads with the rest of the
        // table's property maps.  Keep the raw blob so the parse (and any
        // error) happens when the caller asks for the map, like Java.
        self.name_map_blob = maps
            .default_map()
            .and_then(|map| map.get_value(crate::property::NAME_MAP_PROP))
            .and_then(|value| match value {
                Value::Bytes(bytes) => Some(bytes.clone()),
                _ => None,
            });
        let mut calc_columns: Vec<CalcColumn> = Vec::new();
        let mut column_validators: Vec<Option<ValidationRule>> = vec![None; self.columns.len()];
        for (idx, col) in self.columns.iter_mut().enumerate() {
            col.default_value = None;
            col.required = false;
            col.allow_zero_length = true;
            if col.auto_number {
                continue;
            }
            if col.calculated {
                // "real" data type is in the "result type" property.  Java
                // reads it in `ColumnImpl.create`, before the expression
                // policy is consulted, so this happens either way.
                let result_type = maps
                    .get(&col.name)
                    .and_then(|map| map.get_value(crate::property::RESULT_TYPE_PROP))
                    .and_then(|value| match value {
                        Value::Byte(b) => DataType::from_byte(*b),
                        _ => None,
                    });
                if let Some(result_type) = result_type {
                    col.data_type = result_type;
                }
                if !evaluate_expressions {
                    // `ColumnImpl.reloadPropertiesValidators` sets a null
                    // calculated column evaluator and returns, so the column
                    // keeps the value the row holds
                    continue;
                }
                let expr_str = maps
                    .get(&col.name)
                    .and_then(|map| map.get_value(crate::property::EXPRESSION_PROP))
                    .and_then(|value| match value {
                        Value::Text(s) => Some(s.clone()),
                        _ => None,
                    });
                let result_type = col.data_type;
                let expression = match expr_str {
                    Some(expr_str) => crate::expr::parse_with(
                        &expr_str,
                        crate::expr::ParseOptions {
                            result_type: Some(result_type),
                            ..crate::expr::ParseOptions::default()
                        },
                    ),
                    None => Err(crate::expr::ExprError::Eval(
                        "No expression provided for calculated column".to_string(),
                    )),
                };
                calc_columns.push(CalcColumn {
                    column_index: idx,
                    expression,
                    result_type,
                });
                continue;
            }
            let map = match maps.get(&col.name) {
                Some(map) => map,
                None => continue,
            };
            if evaluate_expressions {
                // `DefaultValue` is an expression in jackcess: it is only
                // built after the policy check
                col.default_value = map
                    .get_trimmed_string(crate::property::DEFAULT_VALUE_PROP)
                    .map(|s| s.to_string());
                // `ValidationRule` / `ValidationText`: the `ColValidatorEvalContext`
                // wraps the required/zero length validators (it delegates to
                // them first, see `InternalColumnValidator.validate`)
                if let Some(rule) = map.get_trimmed_string(crate::property::VALIDATION_RULE_PROP) {
                    let text = map.get_trimmed_string(crate::property::VALIDATION_TEXT_PROP);
                    column_validators[idx] = Some(ValidationRule::field(rule, text));
                }
            }
            col.required = crate::column::to_boolean_value(
                map.get_value_or(crate::property::REQUIRED_PROP, &Value::Bool(false)),
            );
            if col.data_type.is_textual() {
                col.allow_zero_length = crate::column::to_boolean_value(
                    map.get_value_or(crate::property::ALLOW_ZERO_LEN_PROP, &Value::Bool(true)),
                );
            }
        }
        self.column_validators = column_validators;
        // `TableImpl.reloadRowValidator`: the table level rule lives in the
        // object level ("default") property map and is also skipped when
        // expressions are disabled
        self.row_validator = if evaluate_expressions {
            maps.default_map()
                .and_then(|map| map.get_trimmed_string(crate::property::VALIDATION_RULE_PROP))
                .map(|rule| {
                    let text = maps.default_map().and_then(|map| {
                        map.get_trimmed_string(crate::property::VALIDATION_TEXT_PROP)
                    });
                    ValidationRule::row(rule, text)
                })
        } else {
            None
        };
        // calculated columns will need to be re-sorted (their expressions may
        // have changed when their properties were updated)
        self.calc_columns = calc_columns;
        self.calc_sorted = false;
    }

    /// `TableImpl.addRows`'s per-row value massaging for an insert: a null
    /// value of a non-autonumber column is replaced by the column's
    /// `DefaultValue`, and the resulting value is passed through the column
    /// validators (`required` / `allow zero length` / `ValidationRule`).
    ///
    /// `Column.KEEP_VALUE` ([`Value::Keep`]) is only meaningful for an update,
    /// so it is rejected here instead of being written out.
    pub fn prepare_add_row(&self, row: &mut [Value]) -> Result<()> {
        for (ci, col) in self.columns.iter().enumerate() {
            if ci >= row.len() {
                break;
            }
            if col.auto_number {
                continue;
            }
            if row[ci].is_keep() {
                return Err(Error::IllegalArgument(
                    "KEEP_VALUE is not a valid value for a new row".to_string(),
                ));
            }
            let value = row[ci].clone();
            let value = if value.is_null() {
                col.generate_default_value()?
            } else {
                value
            };
            row[ci] = self.validate_column_value(ci, col, value)?;
        }
        Ok(())
    }

    /// The value massaging `TableImpl.updateRow` does: a [`Value::Keep`]
    /// (`Column.KEEP_VALUE`) is replaced by the value the row already holds and
    /// is not validated again, every other value runs the property driven
    /// validators (an update never generates a default value).
    ///
    /// `old_row` is the row as it is stored (from [`Table::read_row_at`]),
    /// padded to the table's column count.
    pub fn prepare_update_row(&self, old_row: &[Value], row: &mut [Value]) -> Result<()> {
        for (ci, col) in self.columns.iter().enumerate() {
            if ci >= row.len() {
                break;
            }
            if col.auto_number {
                continue;
            }
            if row[ci].is_keep() {
                // keep value: restore the old value (`getRowColumn`)
                row[ci] = old_row.get(ci).cloned().unwrap_or(Value::Null);
                continue;
            }
            let value = row[ci].clone();
            row[ci] = self.validate_column_value(ci, col, value)?;
        }
        Ok(())
    }

    /// `Table.newBlob`: a fresh OLE blob builder (Java default method
    /// `return new OleBlob.Builder()`).
    pub fn new_blob(&self) -> crate::ole::OleBlobBuilder {
        crate::ole::OleBlob::builder()
    }

    /// `TableImpl.CalcColEvaluator.willCalculate`: whether an update computes
    /// the value of the given column again (instead of writing the bytes the
    /// row holds).
    pub fn will_calculate(&self, column_index: usize) -> bool {
        self.calc_columns
            .iter()
            .any(|c| c.column_index == column_index)
    }

    /// `TableImpl.CalcColEvaluator.calculate`: assigns the values of the
    /// calculated columns of the row, in dependency order.
    ///
    /// The row values are the expression context, and the result is coerced to
    /// the column's result type (`CalcColEvalContext` +
    /// `ColumnImpl.toInternalValue`).
    pub fn calculate_calculated_columns(&mut self, row: &mut [Value]) -> Result<()> {
        if self.calc_columns.is_empty() {
            return Ok(());
        }
        if !self.calc_sorted {
            let sorted = self.sorted_calc_columns()?;
            self.calc_columns = sorted;
            self.calc_sorted = true;
        }
        let calcs = self.calc_columns.clone();
        self.calculate_with(&calcs, row)
    }

    /// The calculated values of a row without changing the table's sort state
    /// (used by [`Table::preview_add_rows`], which only has `&self`).
    pub fn preview_calculated_row(
        &self,
        row: &mut [Value],
        calcs: Option<&[CalcColumn]>,
    ) -> Result<()> {
        let sorted;
        let calcs = match calcs {
            Some(calcs) => calcs,
            None => {
                if self.calc_columns.is_empty() {
                    return Ok(());
                }
                sorted = self.sorted_calc_columns()?;
                &sorted
            }
        };
        self.calculate_with(calcs, row)
    }

    /// The sorted calculated columns, built without modifying the table.
    pub fn sorted_calc_columns(&self) -> Result<Vec<CalcColumn>> {
        let n = self.calc_columns.len();
        if n <= 1 {
            return Ok(self.calc_columns.clone());
        }
        let mut descs: Vec<Vec<usize>> = Vec::with_capacity(n);
        for i in 0..n {
            let mut out = Vec::new();
            if let Ok(expression) = &self.calc_columns[i].expression {
                for identifier in expression.identifiers() {
                    if !is_this_table_identifier(&identifier, &self.name) {
                        continue;
                    }
                    for (j, calc) in self.calc_columns.iter().enumerate() {
                        if self.columns[calc.column_index]
                            .name
                            .eq_ignore_ascii_case(&identifier.object)
                        {
                            out.push(j);
                        }
                    }
                }
            }
            descs.push(out);
        }
        let mut marks = vec![0u8; n];
        let mut order = Vec::with_capacity(n);
        // build the internal list in reverse so that we maintain the "original"
        // order of items which we do not need to re-arrange
        for i in (0..n).rev() {
            visit_calc_node(i, &descs, &mut marks, &mut order)?;
        }
        Ok(order
            .into_iter()
            .map(|i| self.calc_columns[i].clone())
            .collect())
    }

    fn calculate_with(&self, calcs: &[CalcColumn], row: &mut [Value]) -> Result<()> {
        for calc in calcs {
            let expression = match &calc.expression {
                Ok(expression) => expression,
                Err(e) => return Err(expr_error(e.clone())),
            };
            let context = self.calc_eval_context(row, expression, calc.result_type)?;
            let value = expression
                .eval_column(&context, calc.result_type)
                .map_err(expr_error)?;
            row[calc.column_index] = expr_value_to_value(value);
        }
        Ok(())
    }

    /// Builds the evaluation context of one calculated column from the row
    /// values (`RowEvalContext.getIdentifierValue` resolves the identifiers a
    /// column references against the row).
    fn calc_eval_context(
        &self,
        row: &[Value],
        expression: &crate::expr::Expression,
        result_type: DataType,
    ) -> Result<crate::expr::EvalContext> {
        let mut context = crate::expr::EvalContext::new();
        context.set_table_name(&self.name);
        context.result_type = Some(result_type);
        let mut visited: Vec<usize> = Vec::new();
        for identifier in expression.identifiers() {
            let ci = match self.column_index(&identifier.object) {
                Some(ci) => ci,
                None => continue,
            };
            if visited.contains(&ci) {
                continue;
            }
            visited.push(ci);
            let value = row.get(ci).cloned().unwrap_or(Value::Null);
            let value = value_to_expr_value(&value, &self.time_zone)?;
            context.set_column(&self.columns[ci].name, value);
        }
        Ok(context)
    }

    fn validate_column_value(&self, ci: usize, col: &Column, value: Value) -> Result<Value> {
        // `ColumnImpl.validateValue`: the external validator (set via
        // `Column.setColumnValidator` or the database factory) runs first, then
        // Required / AllowZeroLength and the field `ValidationRule`, exactly
        // like Java's `InternalColumnValidator` chain.
        let context = crate::validator::ColumnValidatorContext::for_column(col)
            .with_database_and_table(&self.db_name, &self.name)
            .with_zone(Some(self.time_zone));
        let rule = self.column_validators.get(ci).and_then(|rule| rule.clone());
        let validator = crate::validator::DefaultColumnValidator::for_column(col)
            .with_rule(rule)
            .with_zone(Some(self.time_zone));
        use crate::validator::ColumnValidator as _;
        validator.validate(&context, value)
    }

    /// The table level row rule (`RowValidatorEvalContext.validate`): the
    /// identifiers resolve against the row's columns, a `false` result (or a
    /// `Null` condition) rejects the row.  Run after the defaults, the auto
    /// numbers and the calculated columns are filled in, before the row is
    /// written (like `TableImpl.addRows` / `TableImpl.updateRow`).
    pub fn validate_row(&self, row: &[Value]) -> Result<()> {
        let rule = match &self.row_validator {
            Some(rule) => rule,
            None => return Ok(()),
        };
        let expression = match &rule.expression {
            Ok(expression) => expression,
            Err(e) => return Err(self.rule_eval_error(e, None)),
        };
        let mut context = crate::expr::EvalContext::new();
        context.set_table_name(&self.name);
        for identifier in expression.identifiers() {
            // `RowEvalContext.getIdentifierValue`: only columns of this table
            // of the current row (and no field properties) are reachable
            let collection_is_this_table = identifier
                .collection
                .as_deref()
                .map(|collection| collection.eq_ignore_ascii_case(&self.name))
                .unwrap_or(true);
            if !collection_is_this_table || identifier.property.is_some() {
                return Err(Error::Jackcess(self.with_error_context(format!(
                    "Cannot access fields outside this table for {identifier}"
                ))));
            }
            let ci = match self.column_index(&identifier.object) {
                Some(ci) => ci,
                None => {
                    return Err(Error::Jackcess(
                        self.with_error_context(format!("Unknown column '{identifier}'")),
                    ))
                }
            };
            let value = row.get(ci).cloned().unwrap_or(Value::Null);
            context.set_column(
                &self.columns[ci].name,
                value_to_expr_value(&value, &self.time_zone)?,
            );
        }
        match expression.eval_condition(&context) {
            Ok(true) => Ok(()),
            Ok(false) => {
                let message = rule.message(|| "Invalid row".to_string());
                Err(Error::InvalidValue(self.with_error_context(&message)))
            }
            Err(e) => Err(self.rule_eval_error(&e, None)),
        }
    }

    /// Maps an expression error of a validation rule to the Java behaviour:
    /// `BaseEvalContext.eval` wraps an `EvalException`/`ParseException` in a
    /// `JackcessException` with the owning object's error context.  A
    /// construct the port does not implement stays an [`Error::Unsupported`].
    fn rule_eval_error(&self, e: &crate::expr::ExprError, col: Option<&Column>) -> Error {
        use crate::expr::ExprError;
        let message = match e {
            ExprError::Parse(m)
            | ExprError::Eval(m)
            | ExprError::Arithmetic(m)
            | ExprError::Unsupported(m) => m.clone(),
        };
        let contextual = match col {
            Some(col) => self.with_column_error_context(&message, col),
            None => self.with_error_context(&message),
        };
        match e {
            ExprError::Unsupported(_) => Error::Unsupported(contextual),
            _ => Error::Jackcess(contextual),
        }
    }

    fn with_column_error_context(&self, msg: &str, col: &Column) -> String {
        format!(
            "{} (Db={};Table={};Column={})",
            msg, self.db_name, self.name, col.name
        )
    }

    pub fn get_format(&self) -> &JetFormat {
        // placeholder; callers pass the format explicitly where needed
        unreachable!()
    }

    /// Reads a table from its table definition page.
    pub fn read(
        ch: &mut PageChannel,
        table_def_page_number: i32,
        name: &str,
        flags: i32,
        format: &JetFormat,
    ) -> Result<Table> {
        // TableImpl.readName decodes names with the database charset
        let charset = ch.charset;
        let mut table_buffer = ch.create_page_buffer();
        ch.read_page(&mut table_buffer, table_def_page_number)?;
        let mut table_buffer = load_complete_table_definition_buffer(ch, table_buffer, format)?;

        let row_count = table_buffer.get_i32_at(format.offset_num_rows)?;
        let last_long_auto_number = table_buffer.get_i32_at(format.offset_next_auto_number)?;
        let last_complex_auto_number = if format.offset_next_complex_auto_number >= 0 {
            table_buffer.get_i32_at(format.offset_next_complex_auto_number as usize)?
        } else {
            0
        };
        let table_type = table_buffer.get_u8_at(format.offset_table_type)?;
        let max_column_count = table_buffer.get_i16_at(format.offset_max_cols)?;
        let max_var_column_count = table_buffer.get_i16_at(format.offset_num_var_cols)?;
        let column_count = table_buffer.get_i16_at(format.offset_num_cols)?;
        let logical_index_count = table_buffer.get_i32_at(format.offset_num_index_slots)?;
        let index_count = table_buffer.get_i32_at(format.offset_num_indexes)?;

        let mut pos = format.offset_owned_pages;
        table_buffer.set_position(pos)?;
        let owned_pages = UsageMap::read_ptr(ch, &mut table_buffer)?;
        pos = format.offset_free_space_pages;
        table_buffer.set_position(pos)?;
        let free_space_pages = UsageMap::read_ptr(ch, &mut table_buffer)?;

        let mut index_datas = Vec::with_capacity(index_count.max(0) as usize);
        for i in 0..index_count.max(0) {
            index_datas.push(IndexData::create(&table_buffer, i, format)?);
        }

        let mut tbl = Table {
            name: name.to_string(),
            table_def_page_number,
            flags,
            row_count,
            last_long_auto_number,
            last_complex_auto_number,
            table_type,
            max_column_count,
            max_var_column_count,
            columns: Vec::with_capacity(column_count.max(0) as usize),
            var_columns: Vec::new(),
            auto_num_columns: Vec::new(),
            index_datas,
            indexes: Vec::new(),
            owned_pages: Some(owned_pages),
            free_space_pages: Some(free_space_pages),
            index_count,
            logical_index_count,
            mod_count: 0,
            write_def_error: None,
            column_umaps: std::collections::HashMap::new(),
            write_row_buffer: None,
            allow_auto_number_insert: None,
            name_map_blob: None,
            // `TableImpl` asks the database for both; a table read outside a
            // `Database` keeps the jackcess defaults (`TableImpl` is only ever
            // constructed by one in Java)
            date_time_type: crate::database::DEFAULT_DATE_TIME_TYPE,
            time_zone: crate::datenums::TimeZone::default(),
            column_order: crate::database::DEFAULT_COLUMN_ORDER,
            db_name: String::new(),
            column_validators: Vec::new(),
            row_validator: None,
            calc_columns: Vec::new(),
            calc_sorted: false,
            definition_only: false,
            created_date: None,
            updated_date: None,
            format: format.clone(),
            error_handler: None,
            db_error_handler: None,
        };

        tbl.read_column_definitions(&mut table_buffer, column_count, format, charset)?;
        tbl.validate_column_defs(format);
        tbl.read_index_definitions(ch, &mut table_buffer, format, charset)?;
        // read column usage maps
        while table_buffer.remaining() >= 2 {
            if !tbl.read_column_usage_maps(ch, &mut table_buffer, format)? {
                break;
            }
        }
        Ok(tbl)
    }

    fn read_column_definitions(
        &mut self,
        table_buffer: &mut ByteBuf,
        column_count: i16,
        format: &JetFormat,
        charset: crate::format::Charset,
    ) -> Result<()> {
        let col_offset = format.offset_index_def_block
            + self.index_count as usize * format.size_index_definition;
        let mut pos = col_offset + column_count as usize * format.size_column_header;
        let mut col_names = Vec::with_capacity(column_count.max(0) as usize);
        for _ in 0..column_count {
            let (name, new_pos) = read_name(table_buffer, pos, format, charset)?;
            col_names.push(name);
            pos = new_pos;
        }

        table_buffer.set_position(pos)?;
        let mut cols = Vec::with_capacity(column_count.max(0) as usize);
        for (i, name) in col_names.into_iter().enumerate() {
            let offset = col_offset + i * format.size_column_header;
            let col_type_byte = table_buffer.get_u8_at(offset)?;
            let mut col = Column::read(table_buffer, offset, name, i, format)?;
            // `ColumnImpl.create`: an unknown type byte becomes
            // UNSUPPORTED_VARLEN or UNSUPPORTED_FIXEDLEN depending on the
            // fixed length flag
            let data_type = match crate::datatype::DataType::from_byte(col_type_byte) {
                Some(t) => t,
                None if (col.flags & crate::column::FIXED_LEN_FLAG_MASK) != 0 => {
                    crate::datatype::DataType::UnsupportedFixedLen
                }
                None => crate::datatype::DataType::UnsupportedVarLen,
            };
            col.set_type(data_type, table_buffer, offset, format)?;
            if col.calculated {
                // calculated columns use the result type property; not
                // implemented, keep the byte type
            }
            cols.push(col);
        }

        // sort by column number
        cols.sort_by_key(|c| c.column_number);
        for (idx, col) in cols.iter_mut().enumerate() {
            col.column_index = idx;
        }
        self.columns = cols;
        self.var_columns = (0..self.columns.len())
            .filter(|i| self.columns[*i].is_variable_length())
            .collect();
        self.var_columns
            .sort_by_key(|i| self.columns[*i].var_len_table_index);
        self.auto_num_columns = (0..self.columns.len())
            .filter(|i| self.columns[*i].auto_number)
            .collect();
        Ok(())
    }

    fn validate_column_defs(&mut self, format: &JetFormat) {
        let max_row_size = format.max_row_size as i32;
        let max_column_count = self.max_column_count;
        let max_var_column_count = self.max_var_column_count;
        let mut error: Option<String> = None;
        for col in &self.columns {
            if col.column_number >= max_column_count {
                error = Some(format!(
                    "column {} has number {}, which is outside the table column count {}",
                    col.name, col.column_number, max_column_count
                ));
            } else if col.is_variable_length() {
                if col.var_len_table_index >= max_var_column_count {
                    error = Some(format!(
                        "variable length column {} has offset index {}, which is outside the table variable length column count {}",
                        col.name, col.var_len_table_index, max_var_column_count
                    ));
                }
            } else if (col.fixed_data_offset as i32 + col.get_length() as i32) > max_row_size {
                error = Some(format!(
                    "fixed length column {} ends at offset {}, which is beyond the maximum row size {}",
                    col.name,
                    col.fixed_data_offset as i32 + col.get_length() as i32,
                    max_row_size
                ));
            }
            if error.is_some() {
                break;
            }
        }
        if let Some(e) = error {
            self.set_write_def_error(e);
        }
    }

    fn set_write_def_error(&mut self, reason: String) {
        if self.write_def_error.is_none() {
            self.write_def_error = Some(format!("Table definition is corrupt, {}", reason));
        }
    }

    fn read_index_definitions(
        &mut self,
        ch: &mut PageChannel,
        table_buffer: &mut ByteBuf,
        format: &JetFormat,
        charset: crate::format::Charset,
    ) -> Result<()> {
        for i in 0..self.index_datas.len() {
            let columns = self.columns.clone();
            self.index_datas[i].read(ch, table_buffer, &columns, format)?;
        }

        for _ in 0..self.logical_index_count {
            let idx = IndexImpl::read(table_buffer, format)?;
            self.indexes.push(idx);
        }
        for i in 0..self.logical_index_count as usize {
            let (name, new_pos) =
                read_name(table_buffer, table_buffer.position(), format, charset)?;
            table_buffer.set_position(new_pos)?;
            self.indexes[i].name = name;
        }
        // record which logical indexes use which data
        for idx in &self.indexes {
            if let Some(data) = self
                .index_datas
                .iter_mut()
                .find(|d| d.number == idx.index_data_number)
            {
                data.logical_index_numbers.push(idx.index_number);
                if idx.is_primary_key() {
                    data.primary_key = true;
                }
            }
        }
        self.indexes.sort_by_key(|i| i.index_number);
        Ok(())
    }

    pub(crate) fn read_column_usage_maps(
        &mut self,
        ch: &mut PageChannel,
        table_buffer: &mut ByteBuf,
        format: &JetFormat,
    ) -> Result<bool> {
        let umap_col_num = table_buffer.get_i16()?;
        if umap_col_num == crate::index::COLUMN_UNUSED {
            return Ok(false);
        }
        let pos = table_buffer.position();
        let next_pos = pos + 2 * crate::usagemap::POINTER_SIZE;
        if next_pos > table_buffer.limit() {
            return Err(Error::Jackcess(
                "Table definition ends inside the usage map pointers of column".into(),
            ));
        }
        let (col_owned, col_free) = match UsageMap::read_ptr(ch, table_buffer)
            .and_then(|owned| UsageMap::read_ptr(ch, table_buffer).map(|free| (owned, free)))
        {
            Ok(pair) => pair,
            Err(_) => {
                // `TableImpl.readColumnUsageMaps`: an unreadable column usage
                // map costs nothing but the free space it tracks, so the table
                // stays usable without it (Java logs a warning and skips the
                // two pointers)
                table_buffer.set_position(next_pos)?;
                return Ok(true);
            }
        };
        self.column_umaps
            .insert(umap_col_num, (col_owned.clone(), col_free.clone()));
        if let Some(col) = self
            .columns
            .iter_mut()
            .find(|c| c.column_number == umap_col_num)
        {
            col.umap_page_number = col_owned.get_table_page_number();
            col.umap_owned_row_number = col_owned.get_row_start() as u8;
            col.umap_free_row_number = col_free.get_row_start() as u8;
        }
        let _ = format;
        Ok(true)
    }

    // -----------------------------------------------------------------
    // reading rows
    // -----------------------------------------------------------------

    /// Iterates over all row ids of this table (all rows, including deleted).
    pub fn row_ids(&self) -> Vec<RowId> {
        let mut out = Vec::new();
        if let Some(pages) = &self.owned_pages {
            let mut page_nums: Vec<i32> = pages.iter_pages().collect();
            page_nums.sort_unstable();
            for page in page_nums {
                out.push((page, -1));
            }
        }
        out
    }

    /// Every row slot of this table, in row id order, with its deleted flag.
    ///
    /// This is the position-only scan a cursor uses to move between rows
    /// (`CursorImpl.findAnotherPosition` / `RowState.isDeleted`): it does not
    /// decode any column values, so an installed [`ErrorHandler`] is not
    /// invoked (Java only calls it when a row is actually read).
    pub fn row_id_statuses(&self, ch: &mut PageChannel) -> Result<Vec<(RowId, bool)>> {
        self.check_data_access()?;
        let format = self.format.clone();
        let mut out = Vec::new();
        let mut page_nums: Vec<i32> = match &self.owned_pages {
            Some(p) => p.iter_pages().collect(),
            None => Vec::new(),
        };
        page_nums.sort_unstable();
        for page_num in page_nums {
            let mut page = ch.create_page_buffer();
            ch.read_page(&mut page, page_num)?;
            if page.get_u8_at(0)? != page_types::DATA {
                continue;
            }
            if page.get_i32_at(4)? != self.table_def_page_number {
                continue;
            }
            let rows_on_page = get_rows_on_data_page(&page, &format);
            for row_num in 0..rows_on_page {
                let raw_start = page.get_i16_at(get_row_start_offset(row_num, &format))?;
                out.push(((page_num, row_num as i32), is_deleted_row(raw_start)));
            }
        }
        Ok(out)
    }

    /// Reads all rows by scanning the owned data pages, following relocated
    /// rows through their overflow pointers.
    ///
    /// Row/column read errors are passed to the table's
    /// [`ErrorHandler`](Table::get_error_handler); with the default handler they
    /// are propagated, otherwise the handler's replacement value is used (a row
    /// is never silently skipped, matching Jackcess 5.0.4).
    pub fn read_all_rows(&self, ch: &mut PageChannel) -> Result<Vec<Option<Vec<Value>>>> {
        Ok(self
            .read_all_rows_with_ids(ch)?
            .into_iter()
            .map(|(_, row)| row)
            .collect())
    }

    /// Reads all rows together with the id of the row header each one lives in
    /// (deleted rows are yielded as `None`).
    pub fn read_all_rows_with_ids(
        &self,
        ch: &mut PageChannel,
    ) -> Result<Vec<(RowId, Option<Vec<Value>>)>> {
        let handler = self.get_error_handler();
        self.read_all_rows_with_ids_using(ch, Some(handler))
    }

    /// [`Table::read_all_rows_with_ids`] with an explicit handler override (a
    /// cursor level handler wins over the table/database handler).  `None`
    /// means "surface every error".
    pub fn read_all_rows_with_ids_using(
        &self,
        ch: &mut PageChannel,
        handler: Option<&dyn ErrorHandler>,
    ) -> Result<Vec<(RowId, Option<Vec<Value>>)>> {
        self.check_data_access()?;
        let format = self.format.clone();
        let mut out = Vec::new();
        let mut page_nums: Vec<i32> = match &self.owned_pages {
            Some(p) => p.iter_pages().collect(),
            None => Vec::new(),
        };
        page_nums.sort_unstable();
        for page_num in page_nums {
            let mut page = ch.create_page_buffer();
            ch.read_page(&mut page, page_num)?;
            if page.get_u8_at(0)? != page_types::DATA {
                continue;
            }
            let tdef = page.get_i32_at(4)?;
            if tdef != self.table_def_page_number {
                continue;
            }
            let rows_on_page = get_rows_on_data_page(&page, &format);
            for row_num in 0..rows_on_page {
                let row_id = (page_num, row_num as i32);
                // note, we need the unmasked value here
                let raw_start = page.get_i16_at(get_row_start_offset(row_num, &format))?;
                if is_deleted_row(raw_start) {
                    out.push((row_id, None));
                    continue;
                }
                let row = self.read_row_at_from_page_using(ch, &page, row_id, handler)?;
                out.push((row_id, Some(row)));
            }
        }
        Ok(out)
    }

    /// Reads the row with the given id, following any overflow pointer.
    pub fn read_row_at(&self, ch: &mut PageChannel, row_id: RowId) -> Result<Vec<Value>> {
        let handler = self.get_error_handler();
        self.read_row_at_using(ch, row_id, Some(handler))
    }

    /// [`Table::read_row_at`] with an explicit handler override.
    pub fn read_row_at_using(
        &self,
        ch: &mut PageChannel,
        row_id: RowId,
        handler: Option<&dyn ErrorHandler>,
    ) -> Result<Vec<Value>> {
        self.check_data_access()?;
        let loc = self.position_at_row_data(ch, row_id)?;
        if loc.deleted {
            return Err(Error::IllegalState(format!(
                "Row is deleted: ({}, {})",
                row_id.0, row_id.1
            )));
        }
        let row_buffer = loc.final_page.narrow(loc.row_start, loc.row_end)?;
        let location = RowStateLocation::new(
            self,
            (loc.header_page_number, loc.header_row_number),
            (loc.final_page_number, loc.final_row_number),
        );
        self.read_row_values(ch, &row_buffer, handler, Some(&location))
    }

    /// Reads a row from an already loaded page, following the overflow pointer
    /// when the header row was relocated.
    fn read_row_at_from_page_using(
        &self,
        ch: &mut PageChannel,
        page: &ByteBuf,
        row_id: RowId,
        handler: Option<&dyn ErrorHandler>,
    ) -> Result<Vec<Value>> {
        let row_num = row_id.1 as usize;
        let raw_start = page.get_i16_at(get_row_start_offset(row_num, &ch.format))?;
        if is_overflow_row(raw_start) {
            return self.read_row_at_using(ch, row_id, handler);
        }
        let row_start = clean_row_start(raw_start) as usize;
        let row_end = find_row_end(page, row_num, &ch.format)?;
        let row_buffer = narrow_to_row(page, row_start, row_end)?;
        let location = RowStateLocation::new(self, row_id, row_id);
        self.read_row_values(ch, &row_buffer, handler, Some(&location))
    }

    pub fn read_row(
        &self,
        ch: &mut PageChannel,
        page: &ByteBuf,
        row_num: usize,
    ) -> Result<Vec<Value>> {
        self.check_data_access()?;
        let row_start = find_row_start(page, row_num, &ch.format)? as usize;
        let row_end = find_row_end(page, row_num, &ch.format)?;
        let row_buffer = page.narrow(row_start, row_end)?;
        // the caller does not pass the row id here, so there is no location for
        // an error handler (`TableImpl.readRow` is an internal helper as well)
        self.read_row_values(ch, &row_buffer, None, None)
    }

    /// Decodes the column values of the row held by the given narrowed row
    /// buffer.
    ///
    /// This is `TableImpl.getRow`'s loop over `getRowColumn`, implemented with
    /// [`RowReader`] (the port of the read side of `TableImpl.RowState`): each
    /// column is decoded inside the equivalent of Java's try block, a failure
    /// is passed to `handler` (when one is provided) and the returned
    /// replacement is stored in the row, otherwise the error is returned.
    ///
    /// The row's null mask is part of that try block, exactly like Java's
    /// `RowState.getNullMask`, which reads it on demand and caches it only
    /// once it was read successfully.  A corrupt null mask is therefore
    /// retried for every column: each one fails again and invokes
    /// `handleRowError` with `columnData` `null` (see the
    /// `corrupt_nullmask_*` fixtures of `tools/verify-errorhandler.ps1`).
    ///
    /// Two different corrupt mask failures are modelled, both of them reported
    /// once per affected column:
    /// * a declared mask which does not fit on the page makes
    ///   `NullMask.read` throw Java's `Buffer.position(int)`
    ///   `IllegalArgumentException` (see [`Table::row_null_mask`]);
    /// * a declared mask which fits on the page but reaches in front of the row
    ///   start still reads (from the page bytes in front of the row), but the
    ///   `position(...)` call invalidates the row buffer mark which
    ///   `narrowToRow` set at the row start, so the `rowBuffer.reset()` at the
    ///   beginning of `getRowColumnData` throws
    ///   `java.nio.InvalidMarkException` ([`Error::InvalidMark`]) for every
    ///   non-null, non-boolean column.
    fn read_row_values(
        &self,
        ch: &mut PageChannel,
        row_buffer: &ByteBuf,
        handler: Option<&dyn ErrorHandler>,
        location: Option<&dyn ErrorLocation>,
    ) -> Result<Vec<Value>> {
        let mut reader = RowReader::new(row_buffer, self.columns.len(), handler, location);
        let mut out = Vec::with_capacity(self.columns.len());
        for column_index in 0..self.columns.len() {
            out.push(reader.get_row_column(self, ch, column_index)?);
        }
        Ok(out)
    }

    /// `ColumnImpl.read` (`getRowColumn`'s final step): turns the raw column
    /// bytes into a value, including the long value indirection of a
    /// `MEMO`/`OLE` column and the raw bytes of an unsupported column type.
    fn read_column_value(&self, ch: &mut PageChannel, col: &Column, data: &[u8]) -> Result<Value> {
        // `UnsupportedColumnImpl.read` returns the raw bytes as a
        // `RawData` wrapper for unknown/unsupported column types
        if col.data_type.is_unsupported() {
            return Ok(Value::Bytes(data.to_vec()));
        }
        if col.data_type.is_long_value() {
            if data.is_empty() {
                return Ok(Value::Null);
            }
            let fmt = ch.format.clone();
            let bin = column::read_long_value(ch, data, &fmt)?;
            // `CalcMemoColImpl.readLongValue` unwraps the calculated value
            let bin = if col.calculated && col.data_type == crate::datatype::DataType::Memo {
                column::unwrap_calculated_value(&bin)
            } else {
                bin
            };
            if col.data_type == crate::datatype::DataType::Memo {
                return Ok(Value::Text(column::decode_text_value(
                    &bin,
                    ch.charset,
                    &ch.format,
                    col.compressed_unicode,
                )));
            }
            return Ok(Value::Bytes(bin));
        }
        col.read_value(
            data,
            crate::bytebuf::ByteOrder::LittleEndian,
            ch.charset,
            &ch.format,
            self.date_time_type,
        )
    }

    /// `TableImpl.getRowColumnData`: locates the bytes of the given column in
    /// the given row buffer (leaves the limit unchanged).
    fn get_row_column_data(
        &self,
        ch: &PageChannel,
        row_buffer: &ByteBuf,
        col: &Column,
        null_mask: &NullMask,
        var_col_offsets: Option<&[i32]>,
        mark_valid: bool,
    ) -> Result<Vec<u8>> {
        // `getRowColumnData` starts with `rowBuffer.reset()`.  Reading the
        // null mask positioned the buffer at `mask_pos`, and a `position(...)`
        // in front of the mark `narrowToRow` set discards that mark, so the
        // reset throws `java.nio.InvalidMarkException` ([`Error::InvalidMark`]).
        if !mark_valid {
            return Err(Error::InvalidMark);
        }
        let format = &ch.format;
        let row_start = row_buffer.position();
        let (col_pos, col_len) = if !col.is_variable_length() {
            let data_start = row_start + format.offset_column_fixed_data_row_offset;
            (
                data_start + col.fixed_data_offset as usize,
                col.fixed_data_size() as usize,
            )
        } else if format.size_row_var_col_offset == 2 {
            // jet 4 rows: the variable column boundaries are a table of
            // 16 bit offsets just before the null mask
            let var_column_offset_pos = (row_buffer.limit() - null_mask.byte_size() - 4)
                - (col.var_len_table_index as usize * 2);
            let var_start = row_buffer.get_u16_at(var_column_offset_pos)? as usize;
            let var_end = row_buffer.get_u16_at(var_column_offset_pos - 2)? as usize;
            (row_start + var_start, var_end - var_start)
        } else {
            // jet 3 rows: the boundaries are a "jump table" of single bytes
            // with 256 byte jumps
            let offsets = match var_col_offsets {
                Some(offsets) => std::borrow::Cow::Borrowed(offsets),
                None => std::borrow::Cow::Owned(read_jump_table_var_col_offsets(
                    row_buffer, row_start, null_mask,
                )?),
            };
            let idx = col.var_len_table_index as usize;
            let var_start = *offsets.get(idx).ok_or_else(|| {
                Error::Jackcess(format!(
                    "Column {} has offset index {}, outside the row jump table",
                    col.name, col.var_len_table_index
                ))
            })?;
            let var_end = *offsets.get(idx + 1).ok_or_else(|| {
                Error::Jackcess(format!(
                    "Column {} has offset index {}, outside the row jump table",
                    col.name, col.var_len_table_index
                ))
            })?;
            (
                row_start + var_start as usize,
                (var_end - var_start) as usize,
            )
        };
        row_buffer.get_bytes_at(col_pos, col_len)
    }

    /// `TableImpl.getRawRowValue`: the raw on-page bytes of the given column of
    /// the given row data buffer, which for a long value column is the
    /// *definition* of the value rather than the value itself.  Returns
    /// `None` when the column is null in the row.
    ///
    /// These are the bytes Java's `keepRawVarValues` re-uses when a column is
    /// updated with `Column.KEEP_VALUE`.  `mark_valid` is the state of the row
    /// buffer mark `getRowColumnData` resets to; it is `false` only for a
    /// corrupt null mask which reaches in front of the row (a well formed row
    /// passes `true`).
    pub fn get_raw_row_value(
        &self,
        ch: &PageChannel,
        row_buffer: &ByteBuf,
        col: &Column,
        null_mask: &NullMask,
        mark_valid: bool,
    ) -> Result<Option<Vec<u8>>> {
        if null_mask.is_null(col.column_number) {
            return Ok(None);
        }
        Ok(Some(self.get_row_column_data(
            ch, row_buffer, col, null_mask, None, mark_valid,
        )?))
    }

    /// `TableImpl.getRowNullMask`: the null mask at the end of the given row
    /// data buffer, together with the absolute page position the mask was read
    /// from.  The row begins with the column count, whose width is
    /// `SIZE_ROW_COLUMN_COUNT` (one byte for Jet 3), and the mask sits in the
    /// last `(columnCount + 7) / 8` bytes of the row.
    ///
    /// Java positions the buffer at `rowBuffer.limit() - nullMask.byteSize()`
    /// (both absolute page offsets, not row relative ones) and a corrupt
    /// column count can make that position negative, which
    /// `Buffer.position(int)` rejects with an
    /// `IllegalArgumentException("newPosition < 0: (N < 0)")` before the mask
    /// bytes are read.  A column count which makes the mask longer than the
    /// row but still within the page is *not* an error here: Java reads the
    /// page bytes in front of the row (the same absolute read the port does),
    /// but the returned position then lies in front of the row buffer mark, so
    /// the caller has to model the `reset()` failure of
    /// `getRowColumnData`.
    fn row_null_mask(row_buffer: &ByteBuf, format: &JetFormat) -> Result<(NullMask, usize)> {
        let column_count = get_var_int_at(
            row_buffer,
            row_buffer.position(),
            format.size_row_column_count,
        )? as usize;
        let mask_size = column_count.div_ceil(8);
        // `rowBuffer.position(rowBuffer.limit() - nullMask.byteSize())`; a
        // negative page offset throws the `Buffer.position(int)` error
        let mask_pos = row_buffer.limit() as i64 - mask_size as i64;
        if mask_pos < 0 {
            return Err(Error::IllegalArgument(format!(
                "newPosition < 0: ({} < 0)",
                mask_pos
            )));
        }
        let mask = NullMask::read(row_buffer, mask_pos as usize, column_count)?;
        Ok((mask, mask_pos as usize))
    }

    // -----------------------------------------------------------------
    // row lookup (the location part of `TableImpl.RowState`)
    // -----------------------------------------------------------------

    /// `TableImpl.positionAtRowHeader`: locates the header row of the given
    /// row id.  The deleted state is determined, but overflow row pointers are
    /// not followed.
    pub fn position_at_row_header(
        &self,
        ch: &mut PageChannel,
        row_id: RowId,
    ) -> Result<RowLocation> {
        self.check_data_access()?;
        let format = ch.format.clone();
        if row_id.0 < 0 || row_id.1 < 0 {
            return Err(Error::IllegalArgument(format!(
                "Given rowId is invalid: ({}, {})",
                row_id.0, row_id.1
            )));
        }

        let owned = self
            .owned_pages
            .as_ref()
            .map(|pages| pages.contains_page_number(row_id.0))
            .unwrap_or(false);
        if !owned {
            return Err(Error::IllegalArgument(format!(
                "Given rowId is invalid for this table: ({}, {})",
                row_id.0, row_id.1
            )));
        }

        let mut header_page = ch.create_page_buffer();
        ch.read_page(&mut header_page, row_id.0)?;
        let rows_on_page = get_rows_on_data_page(&header_page, &format);
        if row_id.1 as usize >= rows_on_page {
            return Err(Error::IllegalArgument(format!(
                "Given rowId is invalid for this table: ({}, {})",
                row_id.0, row_id.1
            )));
        }

        // note, we need the unmasked value here
        let raw_start = header_page.get_i16_at(get_row_start_offset(row_id.1 as usize, &format))?;
        let deleted = is_deleted_row(raw_start);
        let row_start = clean_row_start(raw_start) as usize;
        let row_end = find_row_end(&header_page, row_id.1 as usize, &format)?;

        Ok(RowLocation {
            header_page_number: row_id.0,
            header_row_number: row_id.1,
            header_page: header_page.clone(),
            final_page_number: row_id.0,
            final_row_number: row_id.1,
            final_page: header_page,
            row_start,
            row_end,
            deleted,
        })
    }

    /// `TableImpl.positionAtRowData`: locates the row data, following overflow
    /// row pointers as necessary.  A deleted row yields the header location
    /// with `deleted` set.
    pub fn position_at_row_data(&self, ch: &mut PageChannel, row_id: RowId) -> Result<RowLocation> {
        let mut loc = self.position_at_row_header(ch, row_id)?;
        if loc.deleted {
            return Ok(loc);
        }

        let format = ch.format.clone();
        let mut page = loc.final_page.clone();
        let mut page_number = loc.final_page_number;
        let mut row_num = loc.final_row_number as usize;
        loop {
            // note, we need the unmasked value here as well
            let raw_start = page.get_i16_at(get_row_start_offset(row_num, &format))?;
            let row_end = find_row_end(&page, row_num, &format)?;
            let overflow_row = is_overflow_row(raw_start);
            let row_start = clean_row_start(raw_start) as usize;

            if overflow_row {
                if row_end < row_start + 4 {
                    return Err(Error::Jackcess("invalid overflow row info".into()));
                }
                // overflow page: the "row" data in the current page points to
                // another page/row
                let overflow_row_num = page.get_u8_at(row_start)? as i32;
                let overflow_page_num =
                    crate::byteutil::get3_byte_int_at(&page, row_start + 1, page.order())?;
                let mut next_page = ch.create_page_buffer();
                ch.read_page(&mut next_page, overflow_page_num)?;
                page = next_page;
                page_number = overflow_page_num;
                row_num = overflow_row_num as usize;
                continue;
            }

            let page_limit = page.limit();
            if row_start > row_end || row_end > page_limit {
                return Err(Error::Jackcess(format!(
                    "Invalid row, {} to {} is not within a page of {} bytes",
                    row_start, row_end, page_limit
                )));
            }
            loc.final_page_number = page_number;
            loc.final_row_number = row_num as i32;
            loc.row_start = row_start;
            loc.row_end = row_end;
            loc.final_page = page;
            return Ok(loc);
        }
    }

    // -----------------------------------------------------------------
    // updating / deleting rows
    // -----------------------------------------------------------------

    /// `TableImpl._indexColumns`: the columns which are part of any index, in
    /// the order the index definitions name them (Java's `readIndexDefinitions`
    /// adds each index data's columns to a `LinkedHashSet`, so the order is the
    /// index data order, then the column order within an index).
    pub fn index_column_indexes(&self) -> Vec<usize> {
        let mut indexes: Vec<usize> = Vec::new();
        for index_data in &self.index_datas {
            for index_column in &index_data.columns {
                let ci = index_column.column.column_index;
                if !indexes.contains(&ci) {
                    indexes.push(ci);
                }
            }
        }
        indexes
    }

    /// `TableImpl.deleteRow`: marks the header row deleted, removes the index
    /// entries and decrements the table's row count.  The space the row used
    /// is not reclaimed (jackcess does not compact data pages).
    ///
    /// The values the index entries are removed with come from the *index
    /// columns only*, exactly like the Java method: `deleteRow` decodes
    /// `_indexColumns` and leaves every other slot of the row cache null, so a
    /// corrupt row reports fewer errors through the error handler than a full
    /// row read (and a table without indexes decodes nothing at all).
    pub fn delete_row(&mut self, ch: &mut PageChannel, row_id: RowId) -> Result<()> {
        self.check_data_access()?;
        ch.start_write();
        let result = (|| -> Result<()> {
            let format = ch.format.clone();
            let mut loc = self.position_at_row_header(ch, row_id)?;
            if loc.deleted {
                // don't care about duplicate deletion
                return Ok(());
            }

            // attempt to fill in index column values: only `_indexColumns` are
            // decoded, so a corrupt row reports one error per index column (and
            // a table without indexes reports none at all)
            let row_values = if !self.index_datas.is_empty() {
                let data_loc = self.position_at_row_data(ch, row_id)?;
                let row_buffer = data_loc
                    .final_page
                    .narrow(data_loc.row_start, data_loc.row_end)?;
                let handler = self.error_handler_override();
                let location = RowStateLocation::new(
                    self,
                    (data_loc.header_page_number, data_loc.header_row_number),
                    (data_loc.final_page_number, data_loc.final_row_number),
                );
                let mut reader = RowReader::new(
                    &row_buffer,
                    self.columns.len(),
                    handler.as_deref(),
                    Some(&location),
                );
                for column_index in self.index_column_indexes() {
                    // the returned values are not used: the index updates read
                    // the row cache (`rowState.getRowCacheValues()`)
                    reader.get_row_column(self, ch, column_index)?;
                }
                Some(reader.cache_values())
            } else {
                None
            };

            // the delete flag always gets set in the "header" row (even if the
            // data is on an overflow row)
            let row_index = get_row_start_offset(loc.header_row_number as usize, &format);
            let raw_start = loc.header_page.get_i16_at(row_index)?;
            loc.header_page
                .put_i16_at(row_index, raw_start | DELETED_ROW_MASK | OVERFLOW_ROW_MASK)?;
            write_data_page(ch, &loc.header_page, loc.header_page_number)?;

            // update the indexes
            if let Some(row_values) = row_values {
                for index_data in &mut self.index_datas {
                    index_data.delete_row(ch, &row_values, row_id, &format)?;
                }
            }

            // make sure the table def gets updated
            self.row_count -= 1;
            self.update_table_definition_ch(ch)?;
            Ok(())
        })();
        ch.finish_write()?;
        result
    }

    /// `TableImpl.updateRow`: re-encodes the row, keeping it in place when the
    /// new data fits in the space the old row used and relocating it behind an
    /// overflow pointer otherwise, then updates the index entries.
    ///
    /// A [`Value::Keep`] (`Column.KEEP_VALUE`) column keeps the value the row
    /// already holds, and a kept variable length column also keeps its raw
    /// on-page bytes (Java's `keepRawVarValues`), so a kept `MEMO`/`OLE` value
    /// keeps its existing long-value definition and pages.  An update never
    /// generates a column default value.
    pub fn update_row(
        &mut self,
        ch: &mut PageChannel,
        row_id: RowId,
        row: &[Value],
        tz: &crate::datenums::TimeZone,
    ) -> Result<()> {
        self.check_data_access()?;
        ch.start_write();
        let result = (|| -> Result<()> {
            let format = ch.format.clone();
            let mut loc = self.position_at_row_data(ch, row_id)?;
            if loc.deleted {
                return Err(Error::IllegalState(format!(
                    "Row is deleted: ({}, {})",
                    row_id.0, row_id.1
                )));
            }
            let old_row_size = loc.row_end - loc.row_start;
            let old_row_buffer = loc.final_page.narrow(loc.row_start, loc.row_end)?;
            let handler = self.error_handler_override();
            let location = RowStateLocation::new(
                self,
                (loc.header_page_number, loc.header_row_number),
                (loc.final_page_number, loc.final_row_number),
            );
            let mut reader = RowReader::new(
                &old_row_buffer,
                self.columns.len(),
                handler.as_deref(),
                Some(&location),
            );
            // `TableImpl._indexColumns`: only these columns need an old value
            // for a supplied value (every other column uses the row cache)
            let index_columns = self.index_column_indexes();

            // we need to make sure the row is the right length (fill with
            // null if too short)
            let mut new_row = row.to_vec();
            new_row.resize(self.columns.len(), Value::Null);

            // hang on to the raw on-page values of the variable length columns
            // we are keeping (`TableImpl`'s `keepRawVarValues`).  Re-using them
            // writes the bytes the row already holds, which keeps a long value
            // on the pages it already has instead of writing a new value.
            let mut keep_raw_var_values: std::collections::HashMap<usize, Vec<u8>> =
                std::collections::HashMap::new();

            // the per column value massaging of `TableImpl.updateRow`.  The old
            // value is read lazily, exactly like the Java method: a `KEEP_VALUE`
            // column is read (and a corrupt null mask reports it there), a
            // supplied index column value reads the old value as well, while
            // every other supplied value uses the row cache (which is empty for
            // a freshly positioned row).  Nothing is read up front, so a corrupt
            // mask only reports the columns which are actually read.
            for (ci, col) in self.columns.iter().enumerate() {
                if col.auto_number {
                    // handle these separately (below)
                    continue;
                }
                let row_value = new_row.get(ci).cloned().unwrap_or(Value::Null);
                if row_value.is_keep() {
                    // the bytes the row holds are written again as they are,
                    // which keeps a long value on the pages it already has.  A
                    // calculated column is left out, because the update
                    // computes its value again (the port has no persistent row
                    // state, so it always reads the bytes Java re-uses when its
                    // row cache holds the value).
                    if !self.var_columns.is_empty()
                        && col.is_variable_length()
                        && !self.will_calculate(ci)
                    {
                        // `readRawRowValue` swallows a failed raw read and
                        // returns null, which only costs the byte reuse: the
                        // `getRowColumn` call below reports the same problem
                        // through the error handler.
                        if let Ok(Some(raw)) = reader.get_raw_row_value(self, ch, ci) {
                            keep_raw_var_values.insert(ci, raw);
                        }
                    }
                    // fill in any "keep value" fields (restore old value)
                    new_row[ci] = reader.get_row_column(self, ch, ci)?;
                } else {
                    if index_columns.contains(&ci) {
                        // read (old) row value to help update the indexes
                        let _old_value = reader.get_row_column(self, ch, ci)?;
                    }
                    // the port does not skip the validation of a value which is
                    // identical to the cached object (the port clones values, so
                    // Java's reference comparison cannot be reproduced)
                    new_row[ci] = self.validate_column_value(ci, col, row_value)?;
                }
            }

            // autonumbers are handled separately.  An update keeps the value
            // the row already holds, unless the database allows an explicit
            // auto number insert and the caller gave one (`TableImpl
            // .handleAutoNumbersForUpdate`).  A GUID auto number is never
            // regenerated by an update.  The mutations of the last auto number
            // are deferred until the reader (which borrows the table for the
            // error location) is gone.
            let allow_insert = self
                .allow_auto_number_insert
                .unwrap_or(ch.allow_auto_number_insert);
            let mut next_last_long_auto_number: Option<i32> = None;
            let mut next_last_complex_auto_number: Option<i32> = None;
            for &ci in &self.auto_num_columns {
                let col = &self.columns[ci];
                match col.data_type {
                    crate::datatype::DataType::ComplexType => {
                        match explicit_auto_number_value(&new_row, ci, allow_insert) {
                            Some(n) => {
                                next_last_complex_auto_number = Some(n);
                                new_row[ci] = Value::Long(n);
                            }
                            None => {
                                new_row[ci] = reader.get_row_column(self, ch, ci)?;
                            }
                        }
                    }
                    crate::datatype::DataType::Guid => {
                        match explicit_auto_number_row_value(&new_row, ci, allow_insert) {
                            Some(value) => {
                                new_row[ci] = self.explicit_guid_value(&value, col)?;
                            }
                            None => {
                                new_row[ci] = reader.get_row_column(self, ch, ci)?;
                            }
                        }
                    }
                    _ => match explicit_auto_number_value(&new_row, ci, allow_insert) {
                        Some(n) => {
                            next_last_long_auto_number = Some(n);
                            new_row[ci] = Value::Long(n);
                        }
                        None => {
                            new_row[ci] = reader.get_row_column(self, ch, ci)?;
                        }
                    },
                }
            }

            // `RowState.getRowCacheValues()`: the values the index updates use
            // to remove the row's entries.  The reader and its location borrow
            // the table, so they are dropped before the row is written.
            let old_row_values = reader.cache_values();
            // `rowBuffer.reset()`, used to overwrite the old row in place,
            // throws when a corrupt null mask discarded the mark.  Java reaches
            // that reset even when a handler replaced every failed column, and
            // only *after* the index changes were committed, so the failure is
            // modelled at the same point.
            let row_mark_valid = reader.mark_valid();
            drop(reader);
            if let Some(n) = next_last_long_auto_number {
                adjust_long_auto_number(&mut self.last_long_auto_number, n);
            }
            if let Some(n) = next_last_complex_auto_number {
                if n > self.last_complex_auto_number {
                    self.last_complex_auto_number = n;
                }
            }

            // need to assign calculated values after all the other fields are
            // filled in but before final validation
            self.calculate_calculated_columns(&mut new_row)?;

            // run row validation after the calculated values and before the
            // row (or its index entries) is written
            self.validate_row(&new_row)?;

            // the `DATE` date/time conversion the column applies while writing
            // is also what the index encoder asks for, so resolve it once
            self.normalize_date_values(&mut new_row, tz);

            // generate the new row bytes, padded to at least the old row size
            let mut row_buffer = self
                .write_row_buffer
                .take()
                .unwrap_or_else(|| ch.create_buffer(format.page_size));
            row_buffer.set_limit(format.page_size)?;
            row_buffer.set_position(0)?;
            self.create_row_with_raw(
                ch,
                &new_row,
                &mut row_buffer,
                old_row_size,
                &keep_raw_var_values,
                tz,
            )?;
            let new_row_bytes = row_buffer.remaining_bytes().to_vec();
            self.write_row_buffer = Some(row_buffer);
            let new_row_size = new_row_bytes.len();
            if new_row_size > format.max_row_size {
                return Err(Error::Jackcess(format!(
                    "Row size {} is too large (max {})",
                    new_row_size, format.max_row_size
                )));
            }

            // prepare the index updates: every old entry is removed before any
            // new entry is added (`IndexData.prepareUpdateRow`)
            if !self.index_datas.is_empty() {
                let mut removed = Vec::with_capacity(self.index_datas.len());
                for index_data in &mut self.index_datas {
                    removed.push(index_data.delete_row_entry(
                        ch,
                        &old_row_values,
                        row_id,
                        &format,
                    )?);
                }

                let mut pending = Vec::with_capacity(self.index_datas.len());
                let mut failure = None;
                for index_data in &mut self.index_datas {
                    match index_data.prepare_add_row(ch, &new_row, row_id, &format) {
                        Ok(change) => pending.push(change),
                        Err(e) => {
                            failure = Some(e);
                            break;
                        }
                    }
                }
                if let Some(e) = failure {
                    // restore the entries which were already removed (newest
                    // first, matching the pending change chain)
                    for i in (0..removed.len()).rev() {
                        if let Some(entry) = removed[i].take() {
                            self.index_datas[i].rollback_deleted_row(ch, entry)?;
                        }
                    }
                    return Err(e);
                }
                for (i, change) in pending.into_iter().enumerate() {
                    if let Some(change) = change {
                        // an update replaces the row's old entry, so the
                        // unique entry count is not incremented
                        let replaced_entry = removed[i].is_some();
                        self.index_datas[i].commit_add_row(ch, change, replaced_entry)?;
                    }
                }
            }

            // `TableImpl.updateRow`'s `rowBuffer.reset()`: a corrupt null mask
            // which discarded the row buffer mark makes it throw here, i.e.
            // after the index updates were committed and before the row itself
            // is written (the failed update leaves the index changes in place).
            if !row_mark_valid {
                return Err(Error::InvalidMark);
            }

            if old_row_size >= new_row_size {
                // awesome, slap it in!
                loc.final_page.put_bytes_at(loc.row_start, &new_row_bytes)?;
                write_data_page(ch, &loc.final_page, loc.final_page_number)?;
            } else {
                // bummer, need to find a new page for the data
                let (found_page, found_page_number) = self.find_free_row_space(
                    ch,
                    new_row_size,
                    None,
                    crate::pagechannel::INVALID_PAGE_NUMBER,
                )?;
                let same_page = found_page_number == loc.header_page_number;
                let mut data_page = if same_page {
                    loc.header_page.clone()
                } else {
                    found_page
                };

                // write out the new row data (set the deleted flag on the new
                // data row so that it is ignored during normal table traversal)
                let row_num =
                    add_data_page_row(&mut data_page, new_row_size, &format, DELETED_ROW_MASK)?;
                data_page.put_bytes(&new_row_bytes)?;

                // write the overflow info into the header row and clear out
                // the remaining header data
                let mut header_page = if same_page {
                    data_page.clone()
                } else {
                    loc.header_page.clone()
                };
                let header_start =
                    find_row_start(&header_page, loc.header_row_number as usize, &format)? as usize;
                let header_end =
                    find_row_end(&header_page, loc.header_row_number as usize, &format)?;
                if header_start + 4 > header_end {
                    return Err(Error::Jackcess("invalid overflow row info".into()));
                }
                header_page.put_u8_at(header_start, row_num as u8)?;
                let header_order = header_page.order();
                crate::byteutil::put3_byte_int_at(
                    &mut header_page,
                    found_page_number,
                    header_start + 1,
                    header_order,
                )?;
                header_page.clear_range(header_start + 4, header_end)?;

                // set the overflow flag on the header row
                let header_row_index =
                    get_row_start_offset(loc.header_row_number as usize, &format);
                let raw_start = header_page.get_i16_at(header_row_index)?;
                header_page.put_i16_at(header_row_index, raw_start | OVERFLOW_ROW_MASK)?;

                if same_page {
                    write_data_page(ch, &header_page, found_page_number)?;
                } else {
                    write_data_page(ch, &header_page, loc.header_page_number)?;
                    write_data_page(ch, &data_page, found_page_number)?;
                }
            }

            self.update_table_definition_ch(ch)?;
            Ok(())
        })();
        ch.finish_write()?;
        result
    }

    // -----------------------------------------------------------------
    // writing rows
    // -----------------------------------------------------------------

    /// Resolves the `DateTimeType.DATE` date/time conversion of the date
    /// columns of a row which is about to be written.
    ///
    /// In `DATE` mode Java writes a date/time *column* value through
    /// `DefaultDateTimeFactory.toDateDouble`, which converts the wall clock
    /// time to an instant in the database zone
    /// (`temporalToLocalDateTime(...).atZone(zone).toInstant().toEpochMilli()`)
    /// and then adds the `TimeZone` offset of that instant; it therefore drops
    /// the sub-millisecond part of a [`Value::DateTime`] and moves a local time
    /// which falls in a DST gap forward by the gap length.  The index value
    /// encoder asks the same factory, so Java's row bytes and index entry
    /// agree.  The port's index encoder ([`crate::index`]) is handed the row
    /// array, so the conversion is applied here, once, before both are written:
    /// a `DateTime` becomes the [`Value::DateBits`] holding the double the
    /// `DATE` factory stores.
    ///
    /// In `LOCAL_DATE_TIME` mode (the default) the row is left untouched.
    fn normalize_date_values(&self, row: &mut [Value], tz: &crate::datenums::TimeZone) {
        if self.date_time_type != crate::database::DateTimeType::Date {
            return;
        }
        for (index, value) in row.iter_mut().enumerate() {
            let column = match self.columns.get(index) {
                Some(column) => column,
                None => break,
            };
            if column.data_type != crate::datatype::DataType::ShortDateTime {
                continue;
            }
            if let Value::DateTime(ldt) = *value {
                // `ColumnImpl.DefaultDateTimeFactory.toDateDouble(ldt)`
                let bits = crate::datenums::to_date_double_from_local_datetime(ldt, tz);
                *value = Value::DateBits(bits);
            }
        }
    }

    /// Serializes a row into its on-page byte representation (`createRow`).
    pub fn create_row(
        &mut self,
        ch: &mut PageChannel,
        row: &[Value],
        buffer: &mut ByteBuf,
        min_row_size: usize,
        tz: &crate::datenums::TimeZone,
    ) -> Result<()> {
        self.create_row_with_raw(
            ch,
            row,
            buffer,
            min_row_size,
            &std::collections::HashMap::new(),
            tz,
        )
    }

    /// `TableImpl.createRow(Object[], ByteBuffer, int, Map)`: `raw_var_values`
    /// holds pre-written on-page bytes for variable length columns which are
    /// being kept (Java's `keepRawVarValues`).  A value is re-used when it fits
    /// the remaining row space, which writes the long value definition (and the
    /// inline value) the row already holds instead of taking new long value
    /// pages.
    pub fn create_row_with_raw(
        &mut self,
        ch: &mut PageChannel,
        row: &[Value],
        buffer: &mut ByteBuf,
        min_row_size: usize,
        raw_var_values: &std::collections::HashMap<usize, Vec<u8>>,
        tz: &crate::datenums::TimeZone,
    ) -> Result<()> {
        self.check_data_access()?;
        let format = ch.format.clone();
        let charset = ch.charset;
        let format = &format;
        if let Some(err) = &self.write_def_error {
            return Err(Error::Jackcess(err.clone()));
        }
        if row.iter().any(|v| v.is_keep()) {
            // the update path resolves every KEEP_VALUE first
            return Err(Error::IllegalArgument(
                "KEEP_VALUE is not a valid column value here".to_string(),
            ));
        }
        buffer.put_i16(self.max_column_count)?;
        let mut null_mask = NullMask::new(self.max_column_count as usize);

        let fixed_data_start = buffer.position();
        let mut fixed_data_end = fixed_data_start;
        for col in &self.columns {
            if col.is_variable_length() {
                continue;
            }
            let mut row_value = row.get(col.column_index).cloned().unwrap_or(Value::Null);
            if col.store_in_null_mask() {
                if col.write_to_null_mask(&row_value) {
                    null_mask.mark_not_null(col.column_number);
                }
                row_value = Value::Null;
            }
            if !row_value.is_null() {
                null_mask.mark_not_null(col.column_number);
                buffer.set_position(fixed_data_start + col.fixed_data_offset as usize)?;
                let bytes = col.write_value(
                    &row_value,
                    buffer.order(),
                    charset,
                    format,
                    tz,
                    self.date_time_type,
                )?;
                buffer.put_bytes(&bytes)?;
            }
            buffer.set_position(
                fixed_data_start + col.fixed_data_offset as usize + col.get_length() as usize,
            )?;
            if buffer.position() > fixed_data_end {
                fixed_data_end = buffer.position();
            }
        }
        buffer.set_position(fixed_data_end)?;

        if self.max_var_column_count > 0 {
            let mut max_row_size = format.max_row_size as i64;
            max_row_size -= buffer.position() as i64;
            let trailer_size = null_mask.byte_size() + 4 + (self.max_var_column_count as usize * 2);
            max_row_size -= trailer_size as i64;
            for &vi in &self.var_columns {
                let col = &self.columns[vi];
                if col.data_type.is_long_value()
                    && !row
                        .get(col.column_index)
                        .cloned()
                        .unwrap_or(Value::Null)
                        .is_null()
                {
                    max_row_size -= format.size_long_value_def as i64;
                }
            }

            let mut var_offsets = vec![0i16; self.max_var_column_count as usize];
            let mut var_offsets_index = 0usize;
            let var_columns = self.var_columns.clone();
            for vi in var_columns {
                let col = self.columns[vi].clone();
                let offset = buffer.position() as i16;
                let row_value = row.get(col.column_index).cloned().unwrap_or(Value::Null);
                if !row_value.is_null() {
                    null_mask.mark_not_null(col.column_number);
                    // reuse the bytes of a kept value when they still fit
                    // (`rawVarValues.get(varCol) != null && rawValue.length <=
                    // maxRowSize`)
                    let kept_raw = raw_var_values
                        .get(&vi)
                        .filter(|raw| raw.len() as i64 <= max_row_size);
                    let bytes = if let Some(raw) = kept_raw {
                        raw.clone()
                    } else if col.data_type.is_long_value() {
                        let raw = if col.calculated {
                            // `CalcMemoColImpl.writeLongValue`: the encoded text
                            // is wrapped with the calculated value header
                            col.write_value(
                                &row_value,
                                crate::bytebuf::ByteOrder::LittleEndian,
                                charset,
                                format,
                                tz,
                                self.date_time_type,
                            )?
                        } else {
                            match col.data_type {
                                crate::datatype::DataType::Memo => {
                                    let max_units = col.long_value_max_units(format);
                                    column::encode_text_value(
                                        &row_value,
                                        0,
                                        max_units,
                                        false,
                                        col.compressed_unicode,
                                        charset,
                                        format,
                                    )?
                                }
                                _ => column::to_byte_array(&row_value)?,
                            }
                        };
                        self.write_long_value(ch, vi, &raw, max_row_size.max(0) as usize)?
                    } else {
                        col.write_value(
                            &row_value,
                            crate::bytebuf::ByteOrder::LittleEndian,
                            charset,
                            format,
                            tz,
                            self.date_time_type,
                        )?
                    };
                    max_row_size -= bytes.len() as i64;
                    if col.data_type.is_long_value() {
                        max_row_size += format.size_long_value_def as i64;
                    }
                    if (buffer.position() + bytes.len()) > buffer.limit() {
                        return Err(Error::Jackcess(format!(
                            "Row size {} is too large (max {})",
                            buffer.limit() + bytes.len(),
                            format.max_row_size
                        )));
                    }
                    buffer.put_bytes(&bytes)?;
                }
                while var_offsets_index <= col.var_len_table_index as usize {
                    var_offsets[var_offsets_index] = offset;
                    var_offsets_index += 1;
                }
            }
            while var_offsets_index < var_offsets.len() {
                var_offsets[var_offsets_index] = buffer.position() as i16;
                var_offsets_index += 1;
            }

            let eod = buffer.position();
            if buffer.remaining() < trailer_size {
                return Err(Error::Jackcess(format!(
                    "Row size {} is too large (max {})",
                    eod + trailer_size,
                    format.max_row_size
                )));
            }
            pad_row_buffer(buffer, min_row_size, trailer_size)?;
            buffer.put_i16(eod as i16)?;
            for i in (0..self.max_var_column_count as usize).rev() {
                buffer.put_i16(var_offsets[i])?;
            }
            buffer.put_i16(self.max_var_column_count)?;
        } else {
            pad_row_buffer(buffer, min_row_size, null_mask.byte_size())?;
        }
        null_mask.write(buffer)?;
        buffer.flip();
        Ok(())
    }

    /// Writes a long value (MEMO/OLE) and returns its 12 byte definition
    /// (`LongValueColumnImpl.writeLongValue`).
    fn write_long_value(
        &mut self,
        ch: &mut PageChannel,
        col_idx: usize,
        value: &[u8],
        remaining_row_length: usize,
    ) -> Result<Vec<u8>> {
        let format = ch.format.clone();
        let col = self.columns[col_idx].clone();
        let max_size = col.data_type.get_max_size().max(0) as usize;
        if value.len() > max_size {
            return Err(Error::Jackcess(format!(
                "value too big for column, max {}, got {}",
                max_size,
                value.len()
            )));
        }

        let type_ = if format.size_long_value_def + value.len() <= remaining_row_length
            && value.len() <= format.max_inline_long_value_size
        {
            column::LONG_VALUE_TYPE_THIS_PAGE
        } else if value.len() <= format.max_long_value_row_size {
            column::LONG_VALUE_TYPE_OTHER_PAGE
        } else {
            column::LONG_VALUE_TYPE_OTHER_PAGES
        };

        if type_ == column::LONG_VALUE_TYPE_THIS_PAGE {
            let mut def = ByteBuf::allocate(format.size_long_value_def + value.len());
            def.put_u32((value.len() as u32) | ((type_ as u32) << 24))?;
            def.put_u32(0)?;
            def.put_u32(0)?;
            def.put_bytes(value)?;
            return Ok(def.backing().to_vec());
        }

        if type_ == column::LONG_VALUE_TYPE_OTHER_PAGE {
            let page_num = self.long_value_page(ch, &col, value.len())?;
            let mut page = ch.create_page_buffer();
            ch.read_page(&mut page, page_num)?;
            let row_num = add_data_page_row(&mut page, value.len(), &format, 0)?;
            page.put_bytes(value)?;
            write_data_page(ch, &page, page_num)?;
            let mut def = ByteBuf::allocate(format.size_long_value_def);
            def.put_u32((value.len() as u32) | ((type_ as u32) << 24))?;
            def.put_u8(row_num as u8)?;
            crate::byteutil::put3_byte_int(&mut def, page_num)?;
            def.put_u32(0)?;
            return Ok(def.backing().to_vec());
        }

        // long value spread across multiple pages
        let mut def = ByteBuf::allocate(format.size_long_value_def);
        def.put_u32((value.len() as u32) | ((type_ as u32) << 24))?;
        let mut remaining = value.len();
        let mut pos = 0usize;
        let mut first_page = crate::pagechannel::INVALID_PAGE_NUMBER;
        let mut first_row = 0usize;
        // first long value page (`getLongValuePage(remainingLen)`, capped to a
        // whole long value row)
        let mut cur_page =
            self.long_value_page(ch, &col, remaining.min(format.max_long_value_row_size))?;
        while remaining > 0 {
            let chunk_len = (format.max_long_value_row_size - 4).min(remaining);
            let needs_next = chunk_len < remaining;
            let (next_page_num, next_row_num) = if needs_next {
                // drop the current page from the free space map, then get the
                // page for the next chunk (which may be allocated)
                self.clear_long_value_page(ch, &col, cur_page)?;
                let np = self.long_value_page(ch, &col, (remaining - chunk_len) + 4)?;
                let mut np_buf = ch.create_page_buffer();
                ch.read_page(&mut np_buf, np)?;
                (np, get_rows_on_data_page(&np_buf, &format))
            } else {
                (0, 0)
            };
            let mut page = ch.create_page_buffer();
            ch.read_page(&mut page, cur_page)?;
            let row_num = add_data_page_row(&mut page, chunk_len + 4, &format, 0)?;
            if first_page < 0 {
                first_page = cur_page;
                first_row = row_num;
            }
            page.put_u8(next_row_num as u8)?;
            crate::byteutil::put3_byte_int(&mut page, next_page_num)?;
            page.put_bytes(&value[pos..pos + chunk_len])?;
            write_data_page(ch, &page, cur_page)?;
            pos += chunk_len;
            remaining -= chunk_len;
            if !needs_next {
                break;
            }
            cur_page = next_page_num;
        }
        def.put_u8(first_row as u8)?;
        crate::byteutil::put3_byte_int(&mut def, first_page)?;
        def.put_u32(0)?;
        Ok(def.backing().to_vec())
    }

    /// `UmapLongValueBufferHolder.clear`: drop a long value page from the
    /// column's free space map.
    fn clear_long_value_page(
        &mut self,
        ch: &mut PageChannel,
        col: &Column,
        page_number: i32,
    ) -> Result<()> {
        if page_number == crate::pagechannel::INVALID_PAGE_NUMBER {
            return Ok(());
        }
        if let Some((_owned, free)) = self.column_umaps.get_mut(&col.column_number) {
            free.remove_page_number_forced(ch, page_number, true)?;
        }
        Ok(())
    }

    /// Finds (or creates) a long value data page with room for `data_len`
    /// bytes, using the column's usage maps.
    fn long_value_page(
        &mut self,
        ch: &mut PageChannel,
        col: &Column,
        data_len: usize,
    ) -> Result<i32> {
        let format = ch.format.clone();
        if let Some((owned, free)) = self.column_umaps.get_mut(&col.column_number) {
            // mirror UmapLongValueBufferHolder.findNewPage: use the last page
            // in the free space map if it fits, otherwise drop it from the map
            // and allocate a fresh page
            let mut pages: Vec<i32> = free.iter_pages().collect();
            pages.sort_unstable();
            if let Some(&p) = pages.last() {
                let mut page = ch.create_page_buffer();
                ch.read_page(&mut page, p)?;
                if page.get_u8_at(0)? == page_types::DATA
                    && page.get_u8_at(4)? == b'L'
                    && row_fits_on_data_page(data_len, &page, &format)
                {
                    return Ok(p);
                }
                free.remove_page_number(ch, p)?;
            }
            let p = ch.allocate_new_page()?;
            let mut page = ch.create_page_buffer();
            page.put_u8(page_types::DATA)?;
            page.put_u8(1)?;
            page.put_i16(format.data_page_initial_free_space as i16)?;
            page.put_bytes(b"LVAL")?;
            page.put_i32(0)?;
            page.put_i16(0)?;
            ch.write_page(&page, p)?;
            owned.add_page_number(ch, p)?;
            free.add_page_number(ch, p)?;
            Ok(p)
        } else {
            let p = ch.allocate_new_page()?;
            let mut page = ch.create_page_buffer();
            page.put_u8(page_types::DATA)?;
            page.put_u8(1)?;
            page.put_i16(format.data_page_initial_free_space as i16)?;
            page.put_bytes(b"LVAL")?;
            page.put_i32(0)?;
            page.put_i16(0)?;
            ch.write_page(&page, p)?;
            Ok(p)
        }
    }

    /// Adds a row (`TableImpl.addRows` for a single row).
    ///
    /// This is Java's *non-batch* path
    /// (`TableImpl.addRow` -> `addRows(singleton, false)`): a failure is
    /// reported unchanged, without the `BatchUpdateException` wrapper.
    pub fn add_row(
        &mut self,
        ch: &mut PageChannel,
        row: &[Value],
        tz: &crate::datenums::TimeZone,
    ) -> Result<()> {
        self.add_rows(ch, &[row.to_vec()], tz)
    }

    /// Adds rows, writing data pages and index entries.
    ///
    /// This is jackcess' batch `TableImpl.addRows`: the index pages are
    /// written (and split) once, after the last row.  Call it with a single
    /// row to get `TableImpl.addRow`'s per-row index write/split history
    /// instead (see [`Database::add_rows`](crate::database::Database::add_rows)).
    ///
    /// The primitive reports the original failure, like Java's non-batch
    /// `addRows(rows, false)`; [`Table::add_rows_batch`] wraps it the way the
    /// public `Table.addRows(List)` does.
    pub fn add_rows(
        &mut self,
        ch: &mut PageChannel,
        rows: &[Vec<Value>],
        tz: &crate::datenums::TimeZone,
    ) -> Result<()> {
        self.add_rows_with_ids(ch, rows, tz).map(|_| ())
    }

    /// `Table.addRows(List)` (the jackcess *batch* write): a failure is
    /// reported as `com.healthmarketscience.jackcess.BatchUpdateException`
    /// ([`Error::BatchUpdate`]), exactly like `TableImpl.addRows(rows, true)`.
    ///
    /// The message is Java's `Failed adding rows (Db=...;Table=...): <cause>`,
    /// the update count is the number of rows written before the failure
    /// (cleared to 0 for a write failure, where the state of the rows is
    /// unknown, like Java's `isWriteFailure` path) and the original failure is
    /// kept as the cause, so the root cause is unchanged.
    pub fn add_rows_batch(
        &mut self,
        ch: &mut PageChannel,
        rows: &[Vec<Value>],
        tz: &crate::datenums::TimeZone,
    ) -> Result<()> {
        match self.add_rows_tracked(ch, rows, tz) {
            Ok(_) => Ok(()),
            Err((update_count, cause)) => Err(self.batch_update_error(update_count, cause)),
        }
    }

    /// The `new BatchUpdateException(updateCount, withErrorContext("Failed
    /// adding rows"), cause)` `TableImpl.addRows` throws.
    pub(crate) fn batch_update_error(&self, update_count: usize, cause: Error) -> Error {
        // `TableImpl.addRows`: "we don't really know the status of any of the
        // rows, so clear the update count" for a write failure; any other
        // failure reports the rows which were completed before it
        let update_count = if cause.is_write_failure() {
            0
        } else {
            update_count as i32
        };
        Error::batch_update(
            update_count,
            self.with_error_context("Failed adding rows"),
            cause,
        )
    }

    /// Adds rows and returns the row id of every written row.  The complex
    /// column code needs the row id of a new flat table row (the id of the
    /// value it holds), which is exactly what `addRow` returns.
    pub fn add_rows_with_ids(
        &mut self,
        ch: &mut PageChannel,
        rows: &[Vec<Value>],
        tz: &crate::datenums::TimeZone,
    ) -> Result<Vec<RowId>> {
        self.add_rows_tracked(ch, rows, tz)
            .map_err(|(_, error)| error)
    }

    /// The body of `TableImpl.addRows`: on failure it reports the number of
    /// rows which had been completed, so the batch caller can build Java's
    /// `BatchUpdateException`.
    pub(crate) fn add_rows_tracked(
        &mut self,
        ch: &mut PageChannel,
        rows: &[Vec<Value>],
        tz: &crate::datenums::TimeZone,
    ) -> std::result::Result<Vec<RowId>, (usize, Error)> {
        self.check_data_access().map_err(|e| (0, e))?;
        let mut row_ids = Vec::with_capacity(rows.len());
        if rows.is_empty() {
            return Ok(row_ids);
        }
        ch.start_write();
        let mut update_count = 0i32;
        let result = (|| -> Result<()> {
            let format = ch.format.clone();
            let mut data_page: Option<ByteBuf> = None;
            let mut page_number = -1i32;
            let allow_insert = self
                .allow_auto_number_insert
                .unwrap_or(ch.allow_auto_number_insert);

            for row_in in rows {
                let num_cols = self.columns.len();
                let mut row = row_in.clone();
                row.resize(num_cols, Value::Null);

                // handle the property driven value massaging (default values
                // and column validators) before the auto numbers are assigned,
                // like `TableImpl.addRows`
                self.prepare_add_row(&mut row)?;

                // fill in the auto numbers with local counters and compute the
                // calculated values before the counters are committed: a
                // rejected row leaves them alone, which is the net effect of
                // Java's `restoreAutoNumbersFromAdd`
                let mut next_long_auto_number = self.last_long_auto_number;
                let mut next_complex_auto_number = self.last_complex_auto_number;
                self.assign_auto_numbers_for_add(
                    &mut row,
                    allow_insert,
                    &mut next_long_auto_number,
                    &mut next_complex_auto_number,
                )?;

                // need to assign calculated values after all the other fields
                // are filled in but before final validation
                self.calculate_calculated_columns(&mut row)?;

                // run the row validator before anything is written
                self.validate_row(&row)?;
                self.last_long_auto_number = next_long_auto_number;
                self.last_complex_auto_number = next_complex_auto_number;

                // the `DATE` date/time conversion the column applies while
                // writing is also what the index encoder asks for, so resolve
                // it once
                self.normalize_date_values(&mut row, tz);

                let mut row_buffer = self
                    .write_row_buffer
                    .take()
                    .unwrap_or_else(|| ch.create_buffer(format.page_size));
                row_buffer.set_limit(format.page_size)?;
                row_buffer.set_position(0)?;
                self.create_row(ch, &row, &mut row_buffer, 0, tz)?;
                let row_bytes = row_buffer.remaining_bytes().to_vec();
                self.write_row_buffer = Some(row_buffer);
                let row_len = row_bytes.len();
                if row_len > format.max_row_size {
                    return Err(Error::Jackcess(format!(
                        "Row size {} is too large (max {})",
                        row_len, format.max_row_size
                    )));
                }

                let (new_page, new_page_num) =
                    self.find_free_row_space(ch, row_len, data_page.take(), page_number)?;
                data_page = Some(new_page);
                page_number = new_page_num;

                let row_num = get_rows_on_data_page(data_page.as_ref().unwrap(), &format);

                // index updates
                for idx in &mut self.index_datas {
                    idx.add_row(ch, &row, (page_number, row_num as i32), &format)?;
                }

                add_data_page_row(data_page.as_mut().unwrap(), row_len, &format, 0)?;
                data_page.as_mut().unwrap().put_bytes(&row_bytes)?;
                update_count += 1;
                row_ids.push((page_number, row_num as i32));
            }

            if let Some(page) = data_page.take() {
                write_data_page(ch, &page, page_number)?;
            }
            self.row_count += update_count;
            self.update_table_definition_ch(ch)?;
            Ok(())
        })();
        let finish = ch.finish_write();
        match (result, finish) {
            // `finally { finishWrite(); }`: an error while finishing wins over
            // the body's failure, like a Java finally block
            (_, Err(finish_error)) => Err((update_count as usize, finish_error)),
            (Err(body_error), Ok(())) => Err((update_count as usize, body_error)),
            (Ok(()), Ok(())) => Ok(row_ids),
        }
    }

    /// `ColumnImpl.GuidAutoNumberGenerator.handleInsert`: the explicit GUID
    /// value as it is written.  Java passes the value through `toCharSequence`
    /// (so a `UUID` object becomes its canonical string) and only validates it
    /// when the row bytes are written.  The port rejects a bad value here so the
    /// error carries the same column context as Java's `InvalidValueException`
    /// (`Invalid GUID: <value> (Db=...;Table=...;Column=...)`); a raw-bytes GUID
    /// value (what a read row holds) is kept as it is.
    fn explicit_guid_value(&self, value: &Value, col: &Column) -> Result<Value> {
        let invalid = |text: &str| {
            Error::InvalidValue(
                self.with_column_error_context(&format!("Invalid GUID: {text}"), col),
            )
        };
        match value {
            Value::Uuid(_) => Ok(value.clone()),
            Value::Text(s) => {
                // `ColumnImpl.GUID_PATTERN` + `writeGUIDValue`
                if !crate::column::is_guid_string(s) {
                    return Err(invalid(s));
                }
                Ok(Value::Text(s.clone()))
            }
            other => {
                // `toCharSequence(value).toString()` then `writeGUIDValue`
                Err(invalid(&crate::column::value_message_text(other)))
            }
        }
    }

    /// `TableImpl.handleAutoNumbersForAdd`: fills in the auto number columns
    /// using the caller's counters.  An explicit value is used when the
    /// database allows an auto number insert
    /// (`ColumnImpl.AutoNumberGenerator.handleInsert`), otherwise the next
    /// value is generated.  The counters are only stored back into the table
    /// once the row passed the row validator, so a rejected insert consumes no
    /// auto number (the net effect of Java's `restoreAutoNumbersFromAdd`).
    ///
    /// Every `COMPLEX_TYPE` column of one row shares the same complex value id
    /// (`TableImpl.WriteRowState.getComplexAutoNumber`), which the table stores
    /// at `OFFSET_NEXT_COMPLEX_AUTO_NUMBER`.
    fn assign_auto_numbers_for_add(
        &self,
        row: &mut [Value],
        allow_insert: bool,
        last_long_auto_number: &mut i32,
        last_complex_auto_number: &mut i32,
    ) -> Result<()> {
        if self.auto_num_columns.is_empty() {
            return Ok(());
        }
        let mut complex_auto: Option<i32> = None;
        for &ci in &self.auto_num_columns {
            let col = &self.columns[ci];
            match col.data_type {
                crate::datatype::DataType::Long => {
                    match explicit_auto_number_value(row, ci, allow_insert) {
                        Some(n) => {
                            adjust_long_auto_number(last_long_auto_number, n);
                            row[ci] = Value::Long(n);
                        }
                        None => {
                            *last_long_auto_number += 1;
                            row[ci] = Value::Long(*last_long_auto_number);
                        }
                    }
                }
                crate::datatype::DataType::Guid => {
                    // `GuidAutoNumberGenerator.getNext` generates a fresh
                    // random GUID, `handleInsert` uses the given value.  The
                    // explicit value is only seen when the policy allows it
                    // (`getInputAutoNumberRowValue`); a null, KEEP_VALUE or a
                    // missing value is the same as "nothing given".
                    match explicit_auto_number_row_value(row, ci, allow_insert) {
                        Some(value) => row[ci] = self.explicit_guid_value(&value, col)?,
                        None => row[ci] = Value::Text(crate::column::random_guid_string()?),
                    }
                }
                crate::datatype::DataType::ComplexType => {
                    match explicit_auto_number_value(row, ci, allow_insert) {
                        Some(n) => {
                            if let Some(prev) = complex_auto {
                                if prev != n {
                                    // Java throws InvalidValueException; the
                                    // row cannot be written either way
                                    row[ci] = Value::Null;
                                    continue;
                                }
                            }
                            complex_auto = Some(n);
                            if n > *last_complex_auto_number {
                                *last_complex_auto_number = n;
                            }
                            row[ci] = Value::Long(n);
                        }
                        None => {
                            let id = match complex_auto {
                                Some(id) => id,
                                None => {
                                    *last_complex_auto_number += 1;
                                    let id = *last_complex_auto_number;
                                    complex_auto = Some(id);
                                    id
                                }
                            };
                            row[ci] = Value::Long(id);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The rows [`Table::add_rows`] would write, without modifying the table:
    /// property defaults and column validators applied, padded to the column
    /// count, with the auto numbers filled in, the calculated values computed
    /// and the row validator run.  Used by the foreign key checks, which
    /// jackcess runs after the value massaging, the calculated columns and the
    /// row validator but before the rows are written.
    pub fn preview_add_rows(
        &self,
        rows: &[Vec<Value>],
        allow_insert: bool,
    ) -> Result<Vec<Vec<Value>>> {
        self.check_data_access()?;
        let mut last_long_auto_number = self.last_long_auto_number;
        let mut last_complex_auto_number = self.last_complex_auto_number;
        let calc_columns = if self.calc_columns.is_empty() {
            Vec::new()
        } else {
            self.sorted_calc_columns()?
        };
        let mut out = Vec::with_capacity(rows.len());
        for row_in in rows {
            let mut row = row_in.clone();
            row.resize(self.columns.len(), Value::Null);
            self.prepare_add_row(&mut row)?;
            self.assign_auto_numbers_for_add(
                &mut row,
                allow_insert,
                &mut last_long_auto_number,
                &mut last_complex_auto_number,
            )?;
            self.preview_calculated_row(&mut row, Some(&calc_columns))?;
            // `TableImpl.addRows` runs the row validator after the calculated
            // values and before the database constraints
            self.validate_row(&row)?;
            out.push(row);
        }
        Ok(out)
    }

    /// `TableImpl.isAllowAutoNumberInsert`: the table override falls back to
    /// the database policy (which lives on the page channel).
    pub fn is_allow_auto_number_insert(&self, ch: &PageChannel) -> bool {
        self.allow_auto_number_insert
            .unwrap_or(ch.allow_auto_number_insert)
    }

    /// `TableImpl.setAllowAutoNumberInsert` (`None` restores the database
    /// policy).
    pub fn set_allow_auto_number_insert(&mut self, allow: Option<bool>) {
        self.allow_auto_number_insert = allow;
    }

    /// `FKEnforcer.enforcing`: the foreign key policy is a database setting
    /// (jackcess has no per-table flag), so every table of a database follows
    /// it.
    pub fn enforces_foreign_keys(&self, db: &crate::database::Database) -> bool {
        db.is_enforce_foreign_keys()
    }

    fn find_free_row_space(
        &mut self,
        ch: &mut PageChannel,
        row_size: usize,
        mut data_page: Option<ByteBuf>,
        mut page_number: i32,
    ) -> Result<(ByteBuf, i32)> {
        let format = ch.format.clone();
        let mut modified_page = true;
        if data_page.is_none() {
            if let Some((page, num)) = self.find_last_free_data_page(ch)? {
                data_page = Some(page);
                page_number = num;
                modified_page = false;
            } else {
                return self.new_data_page(ch);
            }
        }
        let page = data_page.unwrap();
        if !row_fits_on_data_page(row_size, &page, &format) {
            if modified_page {
                write_data_page(ch, &page, page_number)?;
            }
            if let Some(free) = self.free_space_pages.as_mut() {
                free.remove_page_number(ch, page_number)?;
            }
            return self.new_data_page(ch);
        }
        Ok((page, page_number))
    }

    fn find_last_free_data_page(&mut self, ch: &mut PageChannel) -> Result<Option<(ByteBuf, i32)>> {
        let format = ch.format.clone();
        let mut page_nums: Vec<i32> = match &self.owned_pages {
            Some(p) => p.iter_pages().collect(),
            None => Vec::new(),
        };
        page_nums.sort_unstable();
        page_nums.reverse();
        for page_num in page_nums {
            let free = self
                .free_space_pages
                .as_ref()
                .map(|f| f.contains_page_number(page_num))
                .unwrap_or(false);
            if !free {
                continue;
            }
            let mut page = ch.create_page_buffer();
            ch.read_page(&mut page, page_num)?;
            if page.get_u8_at(0)? == page_types::DATA {
                return Ok(Some((page, page_num)));
            }
        }
        let _ = format;
        Ok(None)
    }

    fn new_data_page(&mut self, ch: &mut PageChannel) -> Result<(ByteBuf, i32)> {
        let format = ch.format.clone();
        let page_number = ch.allocate_new_page()?;
        let mut page = ch.create_page_buffer();
        page.put_u8(page_types::DATA)?;
        page.put_u8(1)?;
        page.put_i16(format.data_page_initial_free_space as i16)?;
        page.put_i32(self.table_def_page_number)?;
        page.put_i32(0)?;
        page.put_i16(0)?;
        ch.write_page(&page, page_number)?;
        if let Some(owned) = self.owned_pages.as_mut() {
            owned.add_page_number(ch, page_number)?;
        }
        if let Some(free) = self.free_space_pages.as_mut() {
            free.add_page_number(ch, page_number)?;
        }
        Ok((page, page_number))
    }

    /// `updateTableDefinition`: writes row count, auto numbers and index pages.
    pub fn update_table_definition_ch(&mut self, ch: &mut PageChannel) -> Result<()> {
        let format = ch.format.clone();
        let mut tdef_page = ch.create_page_buffer();
        ch.read_page(&mut tdef_page, self.table_def_page_number)?;
        tdef_page.put_i32_at(format.offset_num_rows, self.row_count)?;
        tdef_page.put_i32_at(format.offset_next_auto_number, self.last_long_auto_number)?;
        if format.offset_next_complex_auto_number >= 0 {
            tdef_page.put_i32_at(
                format.offset_next_complex_auto_number as usize,
                self.last_complex_auto_number,
            )?;
        }
        for idx in &mut self.index_datas {
            tdef_page.put_i32_at(idx.unique_entry_count_offset, idx.unique_entry_count)?;
            idx.update(ch)?;
        }
        ch.write_page(&tdef_page, self.table_def_page_number)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `TableImpl.getInputAutoNumberRowValue`: the explicit auto number value of
/// the row, if the database allows inserting one.
fn explicit_auto_number_value(row: &[Value], ci: usize, allow_insert: bool) -> Option<i32> {
    if !allow_insert {
        return None;
    }
    let value = row.get(ci)?;
    if value.is_null() || value.is_keep() {
        // a KEEP_VALUE (like the AUTO_NUMBER marker) behaves as if nothing was
        // given (`getInputAutoNumberRowValue`)
        return None;
    }
    // `ColumnImpl.toNumber(inRowValue).intValue()`
    column::to_number(value).ok().map(|n| n as i32)
}

/// `TableImpl.getInputAutoNumberRowValue` for the value-typed auto number
/// columns (`GUID`): the explicit value of the row, if the policy allows
/// inserting one.  A null, a [`Value::Keep`] (`KEEP_VALUE`) or a missing value
/// answers as if nothing was given, exactly like the number helper above.
fn explicit_auto_number_row_value(row: &[Value], ci: usize, allow_insert: bool) -> Option<Value> {
    if !allow_insert {
        return None;
    }
    let value = row.get(ci)?;
    if value.is_null() || value.is_keep() {
        return None;
    }
    Some(value.clone())
}

/// `TableImpl.adjustLongAutoNumber`: only a larger explicit value moves the
/// last used auto number.
fn adjust_long_auto_number(last: &mut i32, value: i32) {
    if value > *last {
        *last = value;
    }
}

/// `TableImpl.findRowStart`: the (masked) start of the given row in a data
/// page, honoring the format's row location offset (Jet 3 puts the first row
/// entry at offset 10, Jet 4 at offset 14).
pub fn find_row_start(buffer: &ByteBuf, row_num: usize, format: &JetFormat) -> Result<i16> {
    Ok(clean_row_start(
        buffer.get_i16_at(get_row_start_offset(row_num, format))?,
    ))
}

/// `ByteUtil.getUnsignedVarInt(ByteBuffer, int)` read at an absolute offset.
fn get_var_int_at(buffer: &ByteBuf, offset: usize, num_bytes: usize) -> Result<i32> {
    match num_bytes {
        1 => Ok(buffer.get_u8_at(offset)? as i32),
        2 => Ok(buffer.get_u16_at(offset)? as i32),
        3 => crate::byteutil::get3_byte_int_at(buffer, offset, buffer.order()),
        4 => buffer.get_i32_at(offset),
        _ => Err(Error::IllegalArgument(format!(
            "Invalid num bytes {}",
            num_bytes
        ))),
    }
}

/// `TableImpl.readJumpTableVarColOffsets`: the variable length column
/// boundaries of a Jet 3 row, whose "jump table" packs each boundary into a
/// single byte with an implicit 256 byte jump.
///
/// The row ends with the null mask; right before it sits the variable column
/// count, and before that the (reversed) table of one byte offsets.  Only
/// offsets which are a multiple of 256 appear in the jump table: the byte at
/// the end of the table says how many 256 byte jumps into the row the boundary
/// table starts.  Returns `num_var_cols + 1` offsets, relative to the row
/// start.
pub fn read_jump_table_var_col_offsets(
    buffer: &ByteBuf,
    row_start: usize,
    null_mask: &NullMask,
) -> Result<Vec<i32>> {
    let null_mask_size = null_mask.byte_size();
    if buffer.remaining() == 0 {
        return Err(Error::Jackcess("invalid jump table row".into()));
    }
    let row_end = row_start + buffer.remaining() - 1;
    if row_end < null_mask_size {
        return Err(Error::Jackcess("invalid jump table row".into()));
    }
    let num_var_cols = buffer.get_u8_at(row_end - null_mask_size)? as i64;
    let row_len = (row_end as i64) - (row_start as i64) + 1;
    let mut num_jumps = (row_len - 1) / MAX_BYTE as i64;
    let col_offset = row_end as i64 - null_mask_size as i64 - num_jumps - 1;
    // If the last jump is a dummy value, ignore it
    if ((col_offset - row_start as i64 - num_var_cols) / MAX_BYTE as i64) < num_jumps {
        num_jumps -= 1;
    }
    let mut offsets = Vec::with_capacity(num_var_cols as usize + 1);
    let mut jumps_used = 0i64;
    for i in 0..=num_var_cols {
        while jumps_used < num_jumps {
            let jump_pos = (row_end as i64 - null_mask_size as i64 - jumps_used - 1) as usize;
            if i != buffer.get_u8_at(jump_pos)? as i64 {
                break;
            }
            jumps_used += 1;
        }
        let pos = col_offset - i;
        if pos < 0 {
            return Err(Error::Jackcess(format!(
                "Invalid jump table offset {} for variable column {}",
                pos, i
            )));
        }
        offsets
            .push(buffer.get_u8_at(pos as usize)? as i32 + (jumps_used * MAX_BYTE as i64) as i32);
    }
    Ok(offsets)
}

/// `TableImpl.isThisTable(Identifier)`.
fn is_this_table_identifier(identifier: &crate::expr::Identifier, table_name: &str) -> bool {
    match &identifier.collection {
        None => true,
        Some(collection) => collection.eq_ignore_ascii_case(table_name),
    }
}

/// The `TopoSorter` visit of one calculated column.
fn visit_calc_node(
    val: usize,
    descs: &[Vec<usize>],
    marks: &mut [u8],
    out: &mut Vec<usize>,
) -> Result<()> {
    match marks[val] {
        2 => return Ok(()),
        1 => return Err(Error::IllegalState("Cycle detected".into())),
        _ => {}
    }
    marks[val] = 1;
    for &desc in &descs[val] {
        visit_calc_node(desc, descs, marks, out)?;
    }
    marks[val] = 2;
    out.push(val);
    Ok(())
}

/// Converts a stored row value to the expression engine's value
/// (`BaseEvalContext.toValue`).
///
/// A `DATE`-mode value is a `java.util.Date` (`DateExt`) in Java, and
/// `BaseEvalContext.toValue` converts it through
/// `ColumnImpl.toLocalDateTime(Date, db)`, i.e.
/// `LocalDateTime.ofInstant(Instant.ofEpochMilli(getTime()), getZoneId())`.
/// That conversion consults the database zone twice (`fromDateDouble` for the
/// millis, then the instant to local conversion), so it is not the plain stored
/// wall clock time.
fn value_to_expr_value(
    value: &Value,
    tz: &crate::datenums::TimeZone,
) -> Result<crate::expr::ExprValue> {
    use crate::expr::ExprValue;
    Ok(match value {
        Value::Null => ExprValue::Null,
        Value::Bool(b) => ExprValue::Bool(*b),
        Value::Byte(b) => ExprValue::Byte(*b as i8),
        Value::Int(v) => ExprValue::Int(*v),
        Value::Long(v) => ExprValue::Long(*v),
        Value::BigInt(v) => ExprValue::BigInt(*v),
        Value::Decimal { unscaled, scale } => ExprValue::Decimal(
            crate::expr::Decimal::from_unscaled(*unscaled, *scale).map_err(expr_error)?,
        ),
        Value::Float(v) => ExprValue::Double(*v as f64),
        Value::Double(v) => ExprValue::Double(*v),
        Value::Text(s) => ExprValue::Text(s.clone()),
        Value::DateTime(v) => ExprValue::date_time(*v),
        Value::DateBits(bits) => {
            let millis = crate::datenums::from_date_double(*bits, tz);
            let ldt = tz.local_date_time_from_millis(millis);
            ExprValue::date_time(ldt)
        }
        other => {
            return Err(Error::Unsupported(format!(
                "Cannot use a {:?} value in an expression",
                other
            )))
        }
    })
}

/// Converts an expression result to the stored row value.
fn expr_value_to_value(value: crate::expr::ExprValue) -> Value {
    use crate::expr::ExprValue;
    match value {
        ExprValue::Null => Value::Null,
        ExprValue::Bool(b) => Value::Bool(b),
        ExprValue::Byte(v) => Value::Byte(v as u8),
        ExprValue::Int(v) => Value::Int(v),
        ExprValue::Long(v) => Value::Long(v),
        ExprValue::BigInt(v) => Value::BigInt(v),
        ExprValue::Double(v) => Value::Double(v),
        ExprValue::Decimal(d) => Value::Decimal {
            unscaled: d.unscaled(),
            scale: d.scale(),
        },
        ExprValue::Text(s) => Value::Text(s),
        ExprValue::DateTime { value, .. } => Value::DateTime(value),
    }
}

fn expr_error(e: crate::expr::ExprError) -> Error {
    match e {
        crate::expr::ExprError::Unsupported(msg) => Error::Unsupported(msg),
        other => Error::Jackcess(other.to_string()),
    }
}

/// `TableImpl.findRowEnd`: the end of the given row in a data page (the start
/// of the previous row, or the page size for row 0).
pub fn find_row_end(buffer: &ByteBuf, row_num: usize, format: &JetFormat) -> Result<usize> {
    if row_num == 0 {
        Ok(format.page_size)
    } else {
        Ok(clean_row_start(buffer.get_i16_at(get_row_start_offset(row_num - 1, format))?) as usize)
    }
}

pub fn is_deleted_row(row_start: i16) -> bool {
    (row_start & DELETED_ROW_MASK) != 0
}

pub fn is_overflow_row(row_start: i16) -> bool {
    (row_start & OVERFLOW_ROW_MASK) != 0
}

pub fn clean_row_start(row_start: i16) -> i16 {
    row_start & OFFSET_MASK
}

/// `TableImpl.narrowToRow`: validates the row boundaries before narrowing.  A
/// row pointer is masked to 13 bits, so a corrupt one reaches twice as far as
/// the largest page.
fn narrow_to_row(page: &ByteBuf, row_start: usize, row_end: usize) -> Result<ByteBuf> {
    if row_start > row_end || row_end > page.limit() {
        return Err(Error::Jackcess(format!(
            "Invalid row, {} to {} is not within a page of {} bytes",
            row_start,
            row_end,
            page.limit()
        )));
    }
    page.narrow(row_start, row_end)
}

pub fn get_row_start_offset(row_num: usize, format: &JetFormat) -> usize {
    format.offset_row_start + format.size_row_location * row_num
}

pub fn get_row_space_usage(row_size: usize, format: &JetFormat) -> usize {
    row_size + format.size_row_location
}

pub fn get_rows_on_data_page(buffer: &ByteBuf, format: &JetFormat) -> usize {
    if buffer.get_u8_at(0).unwrap_or(0) == page_types::DATA {
        buffer
            .get_u16_at(format.offset_num_rows_on_data_page)
            .unwrap_or(0) as usize
    } else {
        0
    }
}

pub fn row_fits_on_data_page(row_size: usize, buffer: &ByteBuf, format: &JetFormat) -> bool {
    let free_space = buffer.get_i16_at(format.offset_free_space).unwrap_or(0);
    free_space >= get_row_space_usage(row_size, format) as i16
}

pub fn add_data_page_row(
    data_page: &mut ByteBuf,
    row_size: usize,
    format: &JetFormat,
    row_flags: i16,
) -> Result<usize> {
    let row_space_usage = get_row_space_usage(row_size, format) as i16;
    let free_space = data_page.get_i16_at(format.offset_free_space)?;
    data_page.put_i16_at(format.offset_free_space, free_space - row_space_usage)?;
    let row_count = data_page.get_i16_at(format.offset_num_rows_on_data_page)?;
    data_page.put_i16_at(format.offset_num_rows_on_data_page, row_count + 1)?;
    let mut row_location = find_row_end(data_page, row_count as usize, format)? as i16;
    row_location -= row_size as i16;
    data_page.put_i16_at(
        get_row_start_offset(row_count as usize, format),
        row_location | row_flags,
    )?;
    data_page.set_position(row_location as usize)?;
    Ok(row_count as usize)
}

pub fn write_data_page(ch: &mut PageChannel, page: &ByteBuf, page_number: i32) -> Result<()> {
    ch.write_page(page, page_number)
}

fn pad_row_buffer(buffer: &mut ByteBuf, min_row_size: usize, trailer_size: usize) -> Result<()> {
    let pos = buffer.position();
    if pos + trailer_size < min_row_size {
        let pad_size = min_row_size - (pos + trailer_size);
        buffer.clear_range(pos, pos + pad_size)?;
        buffer.advance(pad_size as i64)?;
    }
    Ok(())
}

pub fn read_name(
    buffer: &ByteBuf,
    pos: usize,
    format: &JetFormat,
    charset: crate::format::Charset,
) -> Result<(String, usize)> {
    let len_bytes = format.size_name_length;
    let mut tmp = buffer.dup();
    tmp.set_position(pos)?;
    let name_len = crate::byteutil::get_unsigned_var_int(&mut tmp, len_bytes)? as usize;
    let name_bytes = buffer.get_bytes_at(pos + len_bytes, name_len)?;
    // `TableImpl.readName` decodes with the *database* charset, which for jet 3
    // is the code page named one rather than the format default
    let name = column::decode_uncompressed_text(&name_bytes, 0, name_len, charset);
    Ok((name, pos + len_bytes + name_len))
}

fn load_complete_table_definition_buffer(
    ch: &mut PageChannel,
    mut table_buffer: ByteBuf,
    format: &JetFormat,
) -> Result<ByteBuf> {
    let mut next_page = table_buffer.get_i32_at(format.offset_next_table_def_page)?;
    while next_page != 0 {
        let mut next_buffer = ch.create_page_buffer();
        ch.read_page(&mut next_buffer, next_page)?;
        next_page = next_buffer.get_i32_at(format.offset_next_table_def_page)?;
        // expand the table buffer by PAGE_SIZE - 8 bytes and append
        let mut new_buffer = ByteBuf::allocate(table_buffer.capacity() + format.page_size - 8);
        new_buffer.set_order(table_buffer.order());
        let n = table_buffer.position();
        // capacity is the old buffer length; copy all of it
        let old = table_buffer.into_vec();
        new_buffer.put_bytes(&old)?;
        let _ = n;
        new_buffer.put_bytes(&next_buffer.backing()[8..format.page_size])?;
        new_buffer.flip();
        table_buffer = new_buffer;
    }
    Ok(table_buffer)
}

/// `NullMask`
pub struct NullMask {
    column_count: usize,
    mask: Vec<u8>,
}

impl NullMask {
    pub fn new(column_count: usize) -> NullMask {
        NullMask {
            column_count,
            mask: vec![0u8; column_count.div_ceil(8)],
        }
    }

    pub fn read(buffer: &ByteBuf, offset: usize, column_count: usize) -> Result<NullMask> {
        let size = column_count.div_ceil(8);
        let mask = buffer.get_bytes_at(offset, size)?;
        Ok(NullMask { column_count, mask })
    }

    pub fn write(&self, buffer: &mut ByteBuf) -> Result<()> {
        buffer.put_bytes(&self.mask)
    }

    pub fn is_null(&self, column_number: i16) -> bool {
        let n = column_number as usize;
        n >= self.column_count || (self.mask[n / 8] & (1 << (n % 8))) == 0
    }

    pub fn mark_not_null(&mut self, column_number: i16) {
        let n = column_number as usize;
        self.mask[n / 8] |= 1 << (n % 8);
    }

    pub fn byte_size(&self) -> usize {
        self.mask.len()
    }
}

// ---------------------------------------------------------------------------
// table definition writing (`TableImpl.writeTableDefinition`)
// ---------------------------------------------------------------------------

/// `TableImpl.writeTableDefinitionHeader`
pub fn write_table_definition_header(
    columns: &[ColumnSpec],
    logical_index_count: i32,
    index_count: i32,
    umap_page_number: i32,
    total_table_def_size: usize,
    buffer: &mut ByteBuf,
) -> Result<()> {
    write_table_page_header(buffer)?;
    buffer.put_i32(total_table_def_size as i32)?;
    buffer.put_i32(MAGIC_TABLE_NUMBER)?;
    buffer.put_i32(0)?;
    buffer.put_i32(0)?;
    buffer.put_u8(1)?;
    for _ in 0..15 {
        buffer.put_u8(0)?;
    }
    buffer.put_u8(TYPE_USER)?;
    buffer.put_i16(columns.len() as i16)?;
    buffer.put_i16(column::count_variable_length(columns))?;
    buffer.put_i16(columns.len() as i16)?;
    buffer.put_i32(logical_index_count)?;
    buffer.put_i32(index_count)?;
    buffer.put_u8(0)?;
    crate::byteutil::put3_byte_int(buffer, umap_page_number)?;
    buffer.put_u8(1)?;
    crate::byteutil::put3_byte_int(buffer, umap_page_number)?;
    Ok(())
}

pub fn write_table_page_header(buffer: &mut ByteBuf) -> Result<()> {
    buffer.put_u8(page_types::TABLE_DEF)?;
    buffer.put_u8(0x01)?;
    buffer.put_i16(0)?;
    buffer.put_i32(0)?;
    Ok(())
}

/// Writes a complete table definition buffer to the given page(s).
pub fn write_table_definition_buffer(
    ch: &mut PageChannel,
    buffer: &ByteBuf,
    tdef_page_number: i32,
    reserved_pages: &mut Vec<i32>,
) -> Result<()> {
    let format = ch.format.clone();
    let total_size = buffer.limit();
    if total_size <= format.page_size {
        // easy case, fits on one page.  Java `clear()`s the buffer (position 0,
        // limit == capacity) before writing, so the whole page is written and
        // the bytes past the definition keep whatever the buffer holds there.
        let mut page = buffer.dup();
        page.clear();
        page.put_i16_at(
            format.offset_free_space,
            (format.page_size as i32 - total_size as i32 - 8).max(0) as i16,
        )?;
        ch.write_page(&page, tdef_page_number)?;
        Ok(())
    } else {
        let mut buf = buffer.dup();
        buf.set_position(0)?;
        let mut next_tdef = -1i32;
        // one page buffer reused for every partial page: Java only `clear()`s
        // it between pages, so the bytes past the last chunk keep the previous
        // page's contents
        let mut partial = ch.create_page_buffer();
        while buf.has_remaining() {
            partial.clear();
            if next_tdef == -1 {
                next_tdef = tdef_page_number;
            } else {
                write_table_page_header(&mut partial)?;
            }
            let cur_page = next_tdef;
            let write_len = partial.remaining().min(buf.remaining());
            let chunk = buf.get_bytes(write_len)?;
            partial.put_bytes(&chunk)?;
            if buf.has_remaining() {
                if reserved_pages.is_empty() {
                    next_tdef = ch.allocate_new_page()?;
                } else {
                    next_tdef = reserved_pages.remove(0);
                }
                partial.put_i32_at(format.offset_next_table_def_page, next_tdef)?;
            }
            partial.put_i16_at(
                format.offset_free_space,
                (partial.remaining() as i32 - 8).max(0) as i16,
            )?;
            ch.write_page(&partial, cur_page)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// tests: the byte level effects of row update/delete
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "fixtures"))]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::cursor::Cursor;
    use crate::database::Database;
    use crate::datatype::DataType;
    use crate::format::FileFormat;
    use crate::tablecreator::{IndexSpec, TableSpec};

    fn create_test_db(name: &str) -> (std::path::PathBuf, Database) {
        let path = test_tmp_dir().join(format!(
            "jackcess_rowops_{}_{}.mdb",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    fn make_table(db: &mut Database) {
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(50),
                ColumnSpec::new("Value", DataType::Double),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).expect("create table");
    }

    fn add_rows(db: &mut Database, n: usize) {
        let rows: Vec<Vec<Value>> = (0..n)
            .map(|i| {
                vec![
                    Value::Null,
                    Value::Text(format!("name{}", i)),
                    Value::Double(i as f64 + 0.5),
                ]
            })
            .collect();
        db.add_rows("T", &rows).expect("add rows");
    }

    fn table_page(db: &Database) -> i32 {
        db.table_lookup.get("T").expect("table lookup").0
    }

    fn locate(db: &mut Database, row_id: RowId) -> RowLocation {
        let page = table_page(db);
        db.tables
            .get(&page)
            .expect("table loaded")
            .position_at_row_data(&mut db.channel, row_id)
            .expect("locate row")
    }

    fn read_page(db: &mut Database, page_number: i32) -> ByteBuf {
        let mut page = db.channel.create_page_buffer();
        db.channel
            .read_page(&mut page, page_number)
            .expect("read page");
        page
    }

    fn dir_entry(page: &ByteBuf, format: &JetFormat, row_number: i32) -> i16 {
        page.get_i16_at(get_row_start_offset(row_number as usize, format))
            .expect("dir entry")
    }

    fn tdef_row_count(db: &mut Database, tdef_page: i32) -> i32 {
        let offset = db.format.offset_num_rows;
        read_page(db, tdef_page)
            .get_i32_at(offset)
            .expect("row count")
    }

    fn row_values(db: &mut Database, row_id: RowId) -> Vec<Value> {
        db.read_rows_with_ids("T")
            .expect("rows")
            .into_iter()
            .find(|(id, _)| *id == row_id)
            .and_then(|(_, values)| values)
            .expect("row values")
    }

    #[test]
    fn delete_row_marks_header_and_updates_row_count() {
        let (path, mut db) = create_test_db("delete");
        make_table(&mut db);
        add_rows(&mut db, 6);
        let tdef_page = table_page(&db);
        assert_eq!(tdef_row_count(&mut db, tdef_page), 6);

        let rows = db.read_rows_with_ids("T").expect("rows");
        let target = rows[2].0;
        let free_space_offset = db.format.offset_free_space;
        let free_space_before = read_page(&mut db, target.0)
            .get_i16_at(free_space_offset)
            .expect("free space");

        db.delete_row("T", target).expect("delete row");

        // the deleted flag and the overflow flag are both set in the row
        // directory entry of the header row
        let header = read_page(&mut db, target.0);
        let raw = dir_entry(&header, &db.format, target.1);
        assert!(is_deleted_row(raw), "delete flag not set: {:#06x}", raw);
        assert!(is_overflow_row(raw), "overflow flag not set: {:#06x}", raw);

        // the table definition row count is decremented, but the space the row
        // used is not reclaimed
        assert_eq!(tdef_row_count(&mut db, tdef_page), 5);
        let free_space_after = read_page(&mut db, target.0)
            .get_i16_at(free_space_offset)
            .expect("free space");
        assert_eq!(free_space_after, free_space_before);

        // the row is gone from normal traversal
        let after = db.read_rows_with_ids("T").expect("rows");
        assert!(after
            .iter()
            .any(|(id, values)| *id == target && values.is_none()));
        assert_eq!(after.iter().filter(|(_, v)| v.is_some()).count(), 5);

        // duplicate deletion is a no-op
        db.delete_row("T", target).expect("duplicate delete");
        assert_eq!(tdef_row_count(&mut db, tdef_page), 5);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_row_stays_in_place_when_the_new_data_fits() {
        let (path, mut db) = create_test_db("update_fit");
        make_table(&mut db);
        add_rows(&mut db, 6);

        let rows = db.read_rows_with_ids("T").expect("rows");
        let (row_id, values) = rows[1].clone();
        let mut values = values.expect("row values");
        let before = locate(&mut db, row_id);
        let old_size = before.row_end - before.row_start;

        values[1] = Value::Text("x".to_string());
        db.update_row("T", row_id, &values).expect("update row");

        let after = locate(&mut db, row_id);
        // the row keeps its page/row slot and is padded back to the old size
        assert_eq!(after.final_page_number, before.final_page_number);
        assert_eq!(after.final_row_number, before.final_row_number);
        assert_eq!(after.row_end - after.row_start, old_size);
        let header = read_page(&mut db, after.header_page_number);
        assert!(!is_overflow_row(dir_entry(
            &header,
            &db.format,
            after.header_row_number
        )));

        assert_eq!(row_values(&mut db, row_id)[1], Value::Text("x".to_string()));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn update_row_relocates_through_an_overflow_pointer() {
        let (path, mut db) = create_test_db("update_grow");
        make_table(&mut db);
        add_rows(&mut db, 6);
        let tdef_page = table_page(&db);

        let rows = db.read_rows_with_ids("T").expect("rows");
        let (row_id, values) = rows[1].clone();
        let mut values = values.expect("row values");
        let before = locate(&mut db, row_id);

        // a name which no longer fits in the space the old row used
        values[1] = Value::Text("z".repeat(24));
        db.update_row("T", row_id, &values).expect("update row");

        let after = locate(&mut db, row_id);
        assert_ne!(
            (after.final_page_number, after.final_row_number),
            (before.final_page_number, before.final_row_number)
        );

        // the header row is flagged as an overflow row and holds the pointer
        // to the relocated data, with the rest of the row cleared
        let header = read_page(&mut db, after.header_page_number);
        let header_raw = dir_entry(&header, &db.format, after.header_row_number);
        assert!(is_overflow_row(header_raw));
        assert!(!is_deleted_row(header_raw));
        let header_start = find_row_start(
            &after.header_page,
            after.header_row_number as usize,
            &db.format,
        )
        .expect("row start") as usize;
        let header_end = find_row_end(
            &after.header_page,
            after.header_row_number as usize,
            &db.format,
        )
        .expect("row end");
        assert_eq!(
            after.header_page.get_u8_at(header_start).expect("row num"),
            after.final_row_number as u8
        );
        assert_eq!(
            crate::byteutil::get3_byte_int_at(
                &after.header_page,
                header_start + 1,
                crate::bytebuf::ByteOrder::LittleEndian
            )
            .expect("page num"),
            after.final_page_number
        );
        for i in header_start + 4..header_end {
            assert_eq!(
                after.header_page.get_u8_at(i).expect("cleared byte"),
                0,
                "header row byte {} not cleared",
                i
            );
        }

        // the relocated data row is marked deleted so that normal table
        // traversal ignores it
        let data_raw = dir_entry(
            &read_page(&mut db, after.final_page_number),
            &db.format,
            after.final_row_number,
        );
        assert!(is_deleted_row(data_raw));
        assert!(!is_overflow_row(data_raw));

        // reading the row follows the pointer
        assert_eq!(row_values(&mut db, row_id)[1], Value::Text("z".repeat(24)));

        // an update never increments the index unique entry count (jackcess
        // replaces the row's old entry rather than adding a new one)
        let page = table_page(&db);
        let table = db.tables.get(&page).expect("table loaded");
        assert_eq!(table.index_datas[0].unique_entry_count, 6);
        let offset = table.index_datas[0].unique_entry_count_offset;
        assert_eq!(
            read_page(&mut db, tdef_page)
                .get_i32_at(offset)
                .expect("unique entry count"),
            6
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // -----------------------------------------------------------------
    // KEEP_VALUE (`Column.KEEP_VALUE`) partial row updates
    // -----------------------------------------------------------------

    fn make_keep_table(db: &mut Database) {
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(50),
                ColumnSpec::new("Memo", DataType::Memo),
                ColumnSpec::new("N", DataType::Long),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ))
        .with_index(IndexSpec::new("ByName", vec![("Name".to_string(), true)]).unique())
        .with_index(IndexSpec::new("ByN", vec![("N".to_string(), true)]));
        db.create_table(&spec).expect("create table");
    }

    fn keep_row(name: &str, memo: &str, n: i32) -> Vec<Value> {
        vec![
            Value::Null,
            Value::Text(name.to_string()),
            Value::Text(memo.to_string()),
            Value::Long(n),
        ]
    }

    /// A text long enough to be stored on (several) long value pages.
    fn long_memo_value(len: usize) -> String {
        "abcdefghij".repeat(len / 10)
    }

    fn row_data_bytes(db: &mut Database, row_id: RowId) -> Vec<u8> {
        let page = db.table_lookup.get("TEST").expect("table lookup").0;
        let loc = db
            .tables
            .get(&page)
            .expect("table loaded")
            .position_at_row_data(&mut db.channel, row_id)
            .expect("locate row");
        loc.final_page
            .get_bytes_at(loc.row_start, loc.row_end - loc.row_start)
            .expect("row bytes")
    }

    /// The values of the live (not deleted) rows, in row id order (a relocated
    /// row leaves its old data row behind with the deleted flag set).
    fn live_rows(db: &mut Database) -> Vec<(RowId, Vec<Value>)> {
        db.read_rows_with_ids("Test")
            .expect("rows")
            .into_iter()
            .filter_map(|(id, values)| values.map(|v| (id, v)))
            .collect()
    }

    /// The raw on-page bytes of one column (for a long value the definition).
    fn raw_column_value(db: &mut Database, row_id: RowId, name: &str) -> Option<Vec<u8>> {
        let page = db.table_lookup.get("TEST").expect("table lookup").0;
        let table = db.tables.get(&page).expect("table loaded");
        let col = table.get_column(name).expect("column").clone();
        let loc = table
            .position_at_row_data(&mut db.channel, row_id)
            .expect("locate row");
        let row_buffer = loc
            .final_page
            .narrow(loc.row_start, loc.row_end)
            .expect("row buffer");
        let (null_mask, mask_pos) =
            Table::row_null_mask(&row_buffer, &db.format).expect("null mask");
        let mark_valid = mask_pos >= row_buffer.position();
        table
            .get_raw_row_value(&db.channel, &row_buffer, &col, &null_mask, mark_valid)
            .expect("raw value")
    }

    fn test_file_len(path: &std::path::Path) -> u64 {
        std::fs::metadata(path).expect("metadata").len()
    }

    fn unique_entry_counts(db: &Database) -> Vec<i32> {
        let page = db.table_lookup.get("TEST").expect("table lookup").0;
        db.tables
            .get(&page)
            .expect("table loaded")
            .index_datas
            .iter()
            .map(|d| d.unique_entry_count)
            .collect()
    }

    /// An update which keeps every column rewrites exactly the bytes the row
    /// already holds (fixed values re-encoded from their decoded form, variable
    /// length values and the long value definition re-used as they are).
    #[test]
    fn keep_value_rewrites_the_same_row_bytes() {
        let (path, mut db) = create_test_db("keep_all");
        make_keep_table(&mut db);
        db.add_rows("Test", &[keep_row("alpha", &long_memo_value(3000), 10)])
            .expect("add rows");
        let row_id = live_rows(&mut db)[0].0;
        let before = row_data_bytes(&mut db, row_id);
        let file_len = test_file_len(&path);

        db.update_row_partial(
            "Test",
            row_id,
            &[Value::Keep, Value::Keep, Value::Keep, Value::Keep],
        )
        .expect("keep all");

        assert_eq!(row_data_bytes(&mut db, row_id), before);
        assert_eq!(test_file_len(&path), file_len);
        assert_eq!(
            live_rows(&mut db)[0].1,
            vec![
                Value::Long(1),
                Value::Text("alpha".to_string()),
                Value::Text(long_memo_value(3000)),
                Value::Long(10)
            ]
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A kept long value keeps its definition (the same inline/other page
    /// pointer) and no new long value pages are allocated; replacing the value
    /// does write new pages.
    #[test]
    fn keep_value_preserves_the_long_value_definition() {
        let (path, mut db) = create_test_db("keep_long");
        make_keep_table(&mut db);
        db.add_rows("Test", &[keep_row("alpha", &long_memo_value(3000), 10)])
            .expect("add rows");
        let row_id = live_rows(&mut db)[0].0;

        let memo_before = raw_column_value(&mut db, row_id, "Memo").expect("memo bytes");
        // a long value definition is 12 bytes; the value lives on other pages
        assert_eq!(memo_before.len(), 12);
        let file_len = test_file_len(&path);

        for i in 0..5 {
            db.update_row_partial(
                "Test",
                row_id,
                &[
                    Value::Keep,
                    Value::Text(format!("a{}", i)),
                    Value::Keep,
                    Value::Long(100 + i),
                ],
            )
            .expect("keep memo");
        }

        assert_eq!(
            raw_column_value(&mut db, row_id, "Memo").expect("memo bytes"),
            memo_before
        );
        assert_eq!(test_file_len(&path), file_len);
        assert_eq!(
            live_rows(&mut db)[0].1[2],
            Value::Text(long_memo_value(3000))
        );

        // replacing the value itself writes new long value pages
        let longer = long_memo_value(6000);
        db.update_row_partial(
            "Test",
            row_id,
            &[
                Value::Keep,
                Value::Keep,
                Value::Text(longer.clone()),
                Value::Keep,
            ],
        )
        .expect("replace memo");
        assert_ne!(
            raw_column_value(&mut db, row_id, "Memo").expect("memo bytes"),
            memo_before
        );
        assert!(test_file_len(&path) > file_len);
        assert_eq!(live_rows(&mut db)[0].1[2], Value::Text(longer));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The kept value is the old value for the index updates: an update which
    /// keeps an indexed column replaces the row's entries with the same keys
    /// (the unique entry count does not change), and a rejected unique key
    /// leaves the row and the counts untouched.
    #[test]
    fn keep_value_index_bookkeeping() {
        let (path, mut db) = create_test_db("keep_index");
        make_keep_table(&mut db);
        let rows = vec![
            keep_row("alpha", "memo a", 10),
            keep_row("beta", "memo b", 20),
            keep_row("gamma", "memo c", 30),
        ];
        db.add_rows("Test", &rows).expect("add rows");
        let ids: Vec<RowId> = live_rows(&mut db).into_iter().map(|(id, _)| id).collect();
        let counts = unique_entry_counts(&db);
        assert_eq!(counts, vec![3, 3, 3]);

        // keep the indexed Name and change N: the same entry is replaced
        db.update_row_partial(
            "Test",
            ids[0],
            &[Value::Keep, Value::Keep, Value::Keep, Value::Long(11)],
        )
        .expect("keep name");
        assert_eq!(unique_entry_counts(&db), counts);
        let values = live_rows(&mut db)[0].1.clone();
        assert_eq!(values[1], Value::Text("alpha".to_string()));
        assert_eq!(values[3], Value::Long(11));

        // change the unique Name: the old entry is removed, the new one added
        db.update_row_partial(
            "Test",
            ids[0],
            &[
                Value::Keep,
                Value::Text("delta".to_string()),
                Value::Keep,
                Value::Keep,
            ],
        )
        .expect("change name");
        assert_eq!(unique_entry_counts(&db), counts);
        let values = live_rows(&mut db)[0].1.clone();
        assert_eq!(values[1], Value::Text("delta".to_string()));
        assert_eq!(values[3], Value::Long(11));

        // a duplicate unique Name is rejected: the removed entry is restored
        // and the row is left exactly as it was
        let before = row_data_bytes(&mut db, ids[0]);
        let err = db
            .update_row_partial(
                "Test",
                ids[0],
                &[
                    Value::Keep,
                    Value::Text("beta".to_string()),
                    Value::Keep,
                    Value::Long(99),
                ],
            )
            .expect_err("duplicate unique name must be rejected");
        assert!(!format!("{}", err).is_empty());
        assert_eq!(unique_entry_counts(&db), counts);
        assert_eq!(row_data_bytes(&mut db, ids[0]), before);
        assert_eq!(
            live_rows(&mut db)[0].1,
            vec![
                Value::Long(1),
                Value::Text("delta".to_string()),
                Value::Text("memo a".to_string()),
                Value::Long(11)
            ]
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The three update entry points (`update_row_partial`, `update_value`,
    /// `update_row_from_map`) all honour `Value::Keep`; a keep value is
    /// rejected for a new row, and an update never generates a column default.
    #[test]
    fn keep_value_update_apis() {
        let (path, mut db) = create_test_db("keep_apis");
        make_keep_table(&mut db);
        let rows = vec![
            keep_row("alpha", "memo a", 10),
            keep_row("beta", &long_memo_value(3000), 20),
            keep_row("gamma", "memo c", 30),
        ];
        db.add_rows("Test", &rows).expect("add rows");
        let ids: Vec<RowId> = live_rows(&mut db).into_iter().map(|(id, _)| id).collect();
        let memo_before = raw_column_value(&mut db, ids[1], "Memo").expect("memo bytes");

        // `update_value` (Java `Table.updateValue`) changes one column only
        db.update_value("Test", ids[1], "N", Value::Long(21))
            .expect("update value");
        let values = live_rows(&mut db)[1].1.clone();
        assert_eq!(values[0], Value::Long(2));
        assert_eq!(values[1], Value::Text("beta".to_string()));
        assert_eq!(values[2], Value::Text(long_memo_value(3000)));
        assert_eq!(values[3], Value::Long(21));
        assert_eq!(
            raw_column_value(&mut db, ids[1], "Memo").expect("memo bytes"),
            memo_before
        );

        // `update_row_from_map` (Java `asUpdateRow` + `updateRow`): absent
        // columns are kept, unknown keys are ignored, a null clears a column
        let mut map = std::collections::BTreeMap::new();
        map.insert("Name".to_string(), Value::Text("beta2".to_string()));
        map.insert("NotAColumn".to_string(), Value::Long(7));
        let values = db
            .update_row_from_map("Test", ids[1], &map)
            .expect("update from map");
        assert_eq!(
            values,
            vec![
                Value::Long(2),
                Value::Text("beta2".to_string()),
                Value::Text(long_memo_value(3000)),
                Value::Long(21)
            ]
        );
        assert_eq!(
            raw_column_value(&mut db, ids[1], "Memo").expect("memo bytes"),
            memo_before
        );

        let mut map = std::collections::BTreeMap::new();
        map.insert("Name".to_string(), Value::Null);
        let values = db
            .update_row_from_map("Test", ids[0], &map)
            .expect("clear name");
        assert_eq!(values[0], Value::Long(1));
        assert_eq!(values[1], Value::Null);
        assert_eq!(values[3], Value::Long(10));

        // a KEEP_VALUE is not a value a new row can be given
        let err = db
            .add_rows(
                "Test",
                &[vec![Value::Null, Value::Keep, Value::Null, Value::Null]],
            )
            .expect_err("KEEP_VALUE must be rejected on add");
        assert!(
            format!("{}", err).contains("KEEP_VALUE"),
            "unexpected error: {}",
            err
        );
        assert_eq!(live_rows(&mut db).len(), 3);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A boolean lives in the row's null mask: a kept boolean keeps the old
    /// value.  A kept null keeps the null (an update never generates a column
    /// default), while a value given explicitly still replaces it.
    #[test]
    fn keep_value_keeps_boolean_and_null_values() {
        let (path, mut db) = create_test_db("keep_bool");
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Flag", DataType::Boolean),
                ColumnSpec::new("Dflt", DataType::Long)
                    .put_property(crate::property::DEFAULT_VALUE_PROP, Value::Text("7".into()))
                    .expect("Dflt default"),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).expect("create table");
        db.add_rows(
            "Test",
            &[
                vec![
                    Value::Null,
                    Value::Bool(true),
                    Value::Long(5),
                    Value::Text("a".to_string()),
                ],
                // the null Dflt gets the default value on add
                vec![
                    Value::Null,
                    Value::Bool(false),
                    Value::Null,
                    Value::Text("b".to_string()),
                ],
            ],
        )
        .expect("add rows");
        let ids: Vec<RowId> = live_rows(&mut db).into_iter().map(|(id, _)| id).collect();
        let row_by_id = |db: &mut Database, id: i32| -> Vec<Value> {
            live_rows(db)
                .into_iter()
                .find(|(_, values)| values[0] == Value::Long(id))
                .expect("row")
                .1
        };

        // keep the boolean and the numeric value, change the name
        db.update_row_partial(
            "Test",
            ids[0],
            &[
                Value::Keep,
                Value::Keep,
                Value::Keep,
                Value::Text("a2".to_string()),
            ],
        )
        .expect("keep boolean");
        assert_eq!(
            row_by_id(&mut db, 1),
            vec![
                Value::Long(1),
                Value::Bool(true),
                Value::Long(5),
                Value::Text("a2".to_string())
            ]
        );

        // an explicit null clears the column; a later keep keeps the null (no
        // default value is generated on update)
        db.update_row_partial(
            "Test",
            ids[0],
            &[Value::Keep, Value::Keep, Value::Null, Value::Keep],
        )
        .expect("clear Dflt");
        assert_eq!(row_by_id(&mut db, 1)[2], Value::Null);
        let before = row_data_bytes(&mut db, ids[0]);
        db.update_row_partial(
            "Test",
            ids[0],
            &[Value::Keep, Value::Keep, Value::Keep, Value::Keep],
        )
        .expect("keep null");
        assert_eq!(row_by_id(&mut db, 1)[2], Value::Null);
        assert_eq!(row_data_bytes(&mut db, ids[0]), before);

        // an explicit boolean replaces the kept one
        db.update_row_partial(
            "Test",
            ids[0],
            &[Value::Keep, Value::Bool(false), Value::Keep, Value::Keep],
        )
        .expect("change flag");
        assert_eq!(row_by_id(&mut db, 1)[1], Value::Bool(false));

        // the second row still holds the default it was added with
        assert_eq!(row_by_id(&mut db, 2)[2], Value::Long(7));
        db.update_row_partial(
            "Test",
            ids[1],
            &[
                Value::Keep,
                Value::Keep,
                Value::Keep,
                Value::Text("b2".to_string()),
            ],
        )
        .expect("keep default");
        assert_eq!(row_by_id(&mut db, 2)[2], Value::Long(7));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A default starting with `=` is full expression mode in jackcess; the
    /// port reports it as unsupported instead of writing a different value,
    /// and the rejected insert writes nothing.
    #[test]
    fn expression_default_values_are_unsupported() {
        let (path, mut db) = create_test_db("exprdefault");
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("E", DataType::Long)
                    .put_property(
                        crate::property::DEFAULT_VALUE_PROP,
                        Value::Text("=1+1".into()),
                    )
                    .expect("expression default"),
                ColumnSpec::new("T", DataType::Text)
                    .with_length(10)
                    .put_property(
                        crate::property::DEFAULT_VALUE_PROP,
                        Value::Text("=UCase(\"a\")".into()),
                    )
                    .expect("expression default"),
            ],
        );
        db.create_table(&spec).expect("create table");
        let err = db
            .add_rows("Test", &[vec![Value::Null, Value::Null, Value::Null]])
            .expect_err("expression default must be unsupported");
        match &err {
            Error::Unsupported(msg) => {
                assert!(msg.contains("DefaultValue expression"), "{}", msg)
            }
            other => panic!("unexpected error: {:?}", other),
        }
        assert!(db.read_rows_with_ids("Test").expect("read rows").is_empty());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The property driven column defaults and validators, pinned against the
    /// Java library's values and messages (see `Gen.java` "defaults" and
    /// "required").
    #[test]
    fn default_values_and_validators_match_java() {
        let (path, mut db) = create_test_db("defaults");
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("L", DataType::Long)
                    .put_property(crate::property::DEFAULT_VALUE_PROP, Value::Text("7".into()))
                    .expect("L default"),
                ColumnSpec::new("T", DataType::Text)
                    .with_length(20)
                    .put_property(
                        crate::property::DEFAULT_VALUE_PROP,
                        Value::Text("\"abc\"".into()),
                    )
                    .expect("T default"),
                ColumnSpec::new("M", DataType::Memo)
                    .put_property(
                        crate::property::DEFAULT_VALUE_PROP,
                        Value::Text("\"memo default\"".into()),
                    )
                    .expect("M default"),
                ColumnSpec::new("B", DataType::Boolean)
                    .put_property(
                        crate::property::DEFAULT_VALUE_PROP,
                        Value::Text("TRUE".into()),
                    )
                    .expect("B default"),
                ColumnSpec::new("Req", DataType::Long)
                    .put_property(crate::property::REQUIRED_PROP, Value::Bool(true))
                    .expect("required"),
                // `Required` runs after the default value was generated, so a
                // required column with a default accepts a null input
                ColumnSpec::new("ReqDef", DataType::Long)
                    .put_property(crate::property::REQUIRED_PROP, Value::Bool(true))
                    .expect("required")
                    .put_property(crate::property::DEFAULT_VALUE_PROP, Value::Text("9".into()))
                    .expect("ReqDef default"),
                ColumnSpec::new("Z", DataType::Text)
                    .with_length(20)
                    .put_property(crate::property::ALLOW_ZERO_LEN_PROP, Value::Bool(false))
                    .expect("allow zero length"),
            ],
        );
        db.create_table(&spec).expect("create table");

        let file_name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let required_message = format!(
            "Missing value for required column (Db={};Table=Test;Column=Req)",
            file_name
        );
        let zero_length_message = format!(
            "Zero length string is not allowed (Db={};Table=Test;Column=Z)",
            file_name
        );

        let read_rows = |db: &mut Database| {
            db.read_rows_with_ids("Test")
                .expect("read rows")
                .into_iter()
                .filter_map(|(id, values)| values.map(|values| (id, values)))
                .collect::<Vec<_>>()
        };

        // every default value is applied on add (the required column which has
        // no default still needs a value)
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Long(1),
                Value::Null,
                Value::Null,
            ]],
        )
        .expect("add default row");
        let rows = read_rows(&mut db);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].1,
            vec![
                Value::Long(1),
                Value::Long(7),
                Value::Text("abc".to_string()),
                Value::Text("memo default".to_string()),
                Value::Bool(true),
                Value::Long(1),
                Value::Long(9),
                Value::Null,
            ]
        );

        // explicit values are not replaced by the defaults (a null in the
        // no-zero-length column is allowed, only an empty string is not)
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Long(42),
                Value::Text("xyz".to_string()),
                Value::Text("custom".to_string()),
                Value::Bool(false),
                Value::Long(2),
                Value::Long(3),
                Value::Null,
            ]],
        )
        .expect("add explicit row");

        // a null required value is rejected with the Java message
        let err = db
            .add_rows(
                "Test",
                &[vec![
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                ]],
            )
            .expect_err("null required value");
        assert_eq!(err.to_string(), required_message);
        assert_eq!(read_rows(&mut db).len(), 2);

        // an empty string in a no-zero-length column is rejected
        let err = db
            .add_rows(
                "Test",
                &[vec![
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Null,
                    Value::Long(5),
                    Value::Null,
                    Value::Text(String::new()),
                ]],
            )
            .expect_err("empty string");
        assert_eq!(err.to_string(), zero_length_message);
        assert_eq!(read_rows(&mut db).len(), 2);

        // the rejected inserts consumed no auto number
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Long(4),
                Value::Null,
                Value::Null,
            ]],
        )
        .expect("add after rejects");
        let rows = read_rows(&mut db);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2].1[0], Value::Long(3));

        // an update never generates a default: an explicit null stays null
        let target = rows
            .iter()
            .find(|(_, values)| values[0] == Value::Long(2))
            .expect("row 2")
            .clone();
        let mut values = target.1.clone();
        values[1] = Value::Null;
        values[2] = Value::Text("changed".to_string());
        db.update_row("Test", target.0, &values)
            .expect("update row");
        let rows = read_rows(&mut db);
        let updated = rows
            .iter()
            .find(|(_, values)| values[0] == Value::Long(2))
            .expect("row 2");
        assert_eq!(updated.1[1], Value::Null);
        assert_eq!(updated.1[2], Value::Text("changed".to_string()));
        assert_eq!(updated.1[3], Value::Text("custom".to_string()));

        // the update validators reject a null required value and an empty
        // no-zero-length string, leaving the row unchanged
        let mut bad = updated.1.clone();
        bad[5] = Value::Null;
        let err = db
            .update_row("Test", target.0, &bad)
            .expect_err("update required");
        assert_eq!(err.to_string(), required_message);
        let mut bad = updated.1.clone();
        bad[7] = Value::Text(String::new());
        let err = db
            .update_row("Test", target.0, &bad)
            .expect_err("update zero length");
        assert_eq!(err.to_string(), zero_length_message);
        let rows = read_rows(&mut db);
        let unchanged = rows
            .iter()
            .find(|(_, values)| values[0] == Value::Long(2))
            .expect("row 2");
        assert_eq!(unchanged.1, updated.1);

        // a property save reloads the cached table's validators and default
        // values (`TableImpl.propertiesUpdated`)
        let mut maps = db.get_table_properties_by_name("Test").expect("properties");
        maps.get_mut("L")
            .put_value(
                crate::property::DEFAULT_VALUE_PROP,
                Value::Text("11".into()),
            )
            .expect("put default");
        maps.save(&mut db).expect("save properties");
        db.add_rows(
            "Test",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Long(6),
                Value::Null,
                Value::Null,
            ]],
        )
        .expect("add after property update");
        let rows = read_rows(&mut db);
        let last = rows.last().expect("last row");
        assert_eq!(last.1[1], Value::Long(11));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The `ValidationRule` / `ValidationText` properties of the `validate` and
    /// `validateupdate` scenarios: a numeric field rule with text, a function
    /// based field rule with the default message, and a two column row rule.
    fn validation_spec() -> TableSpec {
        TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Num", DataType::Long)
                    .put_property(
                        crate::property::VALIDATION_RULE_PROP,
                        Value::Text("[Num] >= 0".into()),
                    )
                    .expect("Num rule")
                    .put_property(
                        crate::property::VALIDATION_TEXT_PROP,
                        Value::Text("Num must be >= 0".into()),
                    )
                    .expect("Num text"),
                ColumnSpec::new("Code", DataType::Text)
                    .with_length(10)
                    .put_property(
                        crate::property::VALIDATION_RULE_PROP,
                        Value::Text("Len([Code]) = 3".into()),
                    )
                    .expect("Code rule"),
                ColumnSpec::new("Start", DataType::ShortDateTime),
                ColumnSpec::new("End", DataType::ShortDateTime),
            ],
        )
        .put_property(
            crate::property::VALIDATION_RULE_PROP,
            Value::Text("[Start] <= [End]".into()),
        )
        .expect("row rule")
        .put_property(
            crate::property::VALIDATION_TEXT_PROP,
            Value::Text("Start must not be after End".into()),
        )
        .expect("row text")
    }

    fn validation_date(month: u32, day: u32) -> Value {
        Value::DateTime(crate::datenums::LocalDateTime::new(
            2020, month, day, 0, 0, 0, 0,
        ))
    }

    fn validation_row(num: i32, code: &str, start: Value, end: Value) -> Vec<Value> {
        vec![
            Value::Null,
            Value::Long(num),
            Value::Text(code.into()),
            start,
            end,
        ]
    }

    /// A rejected insert/update of a rule violation leaves the file and the
    /// auto number counter untouched, and reports the exact Java message.
    #[test]
    fn validation_rules_reject_with_java_messages() {
        let (path, mut db) = create_test_db("validate_rules");
        db.create_table(&validation_spec()).expect("create table");
        let db_name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let column_message = |text: &str, column: &str| {
            format!("{} (Db={};Table=Test;Column={})", text, db_name, column)
        };
        let table_message = |text: &str| format!("{} (Db={};Table=Test)", text, db_name);

        db.add_rows(
            "Test",
            &[validation_row(
                1,
                "abc",
                validation_date(1, 1),
                validation_date(1, 2),
            )],
        )
        .expect("valid row");
        db.flush().expect("flush");
        let before = std::fs::read(&path).expect("read file");

        // the numeric field rule rejects a negative value with its text
        let err = db
            .add_rows(
                "Test",
                &[validation_row(
                    -5,
                    "abc",
                    validation_date(1, 1),
                    validation_date(1, 2),
                )],
            )
            .expect_err("negative Num");
        assert_eq!(err.to_string(), column_message("Num must be >= 0", "Num"));
        assert_eq!(db.read_rows_with_ids("Test").expect("rows").len(), 1);

        // the function rule rejects a 2 character code with the default Java
        // message (no ValidationText)
        let err = db
            .add_rows(
                "Test",
                &[validation_row(
                    5,
                    "ab",
                    validation_date(1, 1),
                    validation_date(1, 2),
                )],
            )
            .expect_err("short Code");
        assert_eq!(
            err.to_string(),
            column_message("Invalid column value 'ab'", "Code")
        );

        // the row rule rejects Start after End
        let err = db
            .add_rows(
                "Test",
                &[validation_row(
                    5,
                    "abc",
                    validation_date(1, 2),
                    validation_date(1, 1),
                )],
            )
            .expect_err("Start after End");
        assert_eq!(
            err.to_string(),
            table_message("Start must not be after End")
        );

        // no rejected insert changed the file
        db.flush().expect("flush");
        assert_eq!(std::fs::read(&path).expect("read file"), before);
        assert_eq!(db.read_rows_with_ids("Test").expect("rows").len(), 1);

        // the rejected inserts consumed no auto number
        db.add_rows(
            "Test",
            &[validation_row(
                2,
                "def",
                validation_date(2, 1),
                validation_date(2, 2),
            )],
        )
        .expect("second valid row");
        let rows = db.read_rows_with_ids("Test").expect("rows");
        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|(_, values)| values.as_ref())
            .filter_map(|values| values[0].as_i64())
            .collect();
        assert_eq!(ids, vec![1, 2]);

        let row1 = rows
            .iter()
            .find(|(_, values)| {
                values
                    .as_ref()
                    .and_then(|values| values[0].as_i64())
                    .map(|id| id == 1)
                    .unwrap_or(false)
            })
            .and_then(|(row_id, values)| values.as_ref().map(|values| (*row_id, values.clone())))
            .expect("row 1");
        let row2 = rows
            .iter()
            .find(|(_, values)| {
                values
                    .as_ref()
                    .and_then(|values| values[0].as_i64())
                    .map(|id| id == 2)
                    .unwrap_or(false)
            })
            .and_then(|(row_id, values)| values.as_ref().map(|values| (*row_id, values.clone())))
            .expect("row 2");

        db.flush().expect("flush");
        let before_updates = std::fs::read(&path).expect("read file");

        // a rejected update of the numeric rule
        let mut bad = row1.1.clone();
        bad[1] = Value::Long(-1);
        let err = db
            .update_row("Test", row1.0, &bad)
            .expect_err("negative Num update");
        assert_eq!(err.to_string(), column_message("Num must be >= 0", "Num"));
        // a rejected update which only the row rule rejects
        let mut bad = row2.1.clone();
        bad[3] = validation_date(3, 1);
        bad[4] = validation_date(2, 1);
        let err = db
            .update_row("Test", row2.0, &bad)
            .expect_err("Start after End update");
        assert_eq!(
            err.to_string(),
            table_message("Start must not be after End")
        );
        db.flush().expect("flush");
        assert_eq!(std::fs::read(&path).expect("read file"), before_updates);
        let rows = db.read_rows_with_ids("Test").expect("rows");
        let unchanged = rows
            .iter()
            .find(|(_, values)| {
                values
                    .as_ref()
                    .and_then(|values| values[0].as_i64())
                    .map(|id| id == 1)
                    .unwrap_or(false)
            })
            .and_then(|(_, values)| values.clone())
            .expect("row 1");
        assert_eq!(unchanged, row1.1);

        // valid updates are accepted and the later reads see them
        let mut good = row1.1.clone();
        good[1] = Value::Long(7);
        good[2] = Value::Text("new".into());
        db.update_row("Test", row1.0, &good).expect("valid update");
        let rows = db.read_rows_with_ids("Test").expect("rows");
        let stored = rows
            .iter()
            .find(|(_, values)| {
                values
                    .as_ref()
                    .and_then(|values| values[0].as_i64())
                    .map(|id| id == 1)
                    .unwrap_or(false)
            })
            .and_then(|(_, values)| values.clone())
            .expect("row 1");
        assert_eq!(stored[1], Value::Long(7));
        assert_eq!(stored[2], Value::Text("new".into()));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `Database.setEvaluateExpressions(false)` disables the expression based
    /// validation rules exactly like the defaults and calculated columns: the
    /// rules are not parsed, so even a violating row is written as given.
    #[test]
    fn evaluate_expressions_gates_validation_rules() {
        let (path, mut db) = create_test_db("validate_off");
        db.set_evaluate_expressions(false);
        db.create_table(&validation_spec()).expect("create table");
        db.add_rows(
            "Test",
            &[validation_row(
                -5,
                "ab",
                validation_date(1, 2),
                validation_date(1, 1),
            )],
        )
        .expect("rules are disabled");
        let rows = db.read_rows_with_ids("Test").expect("rows");
        assert_eq!(rows.len(), 1);
        let values = rows[0].1.as_ref().expect("row");
        assert_eq!(values[1], Value::Long(-5));
        assert_eq!(values[2], Value::Text("ab".into()));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A rule which refers to another column of a field rule is rejected the
    /// way `ColValidatorEvalContext.getIdentifierValue` does; a malformed rule
    /// is reported on evaluation (not on property load).
    #[test]
    fn validation_rule_access_and_parse_errors_match_java() {
        let (path, mut db) = create_test_db("validate_errors");
        db.create_table(&TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("Other", DataType::Long),
                ColumnSpec::new("Num", DataType::Long)
                    .put_property(
                        crate::property::VALIDATION_RULE_PROP,
                        Value::Text("[Other] > 0".into()),
                    )
                    .expect("field rule"),
            ],
        ))
        .expect("create table");
        let db_name = path
            .file_name()
            .expect("file name")
            .to_string_lossy()
            .into_owned();
        let err = db
            .add_rows("Test", &[vec![Value::Long(1), Value::Long(1)]])
            .expect_err("a field rule cannot read another column");
        assert_eq!(
            err.to_string(),
            format!(
                "Cannot access other fields for [Other] \
                 (Db={};Table=Test;Column=Num)",
                db_name
            )
        );
        assert!(db.read_rows_with_ids("Test").expect("rows").is_empty());

        // a malformed rule is reported when the row is validated
        let mut maps = db.get_table_properties_by_name("Test").expect("properties");
        maps.get_mut("Num")
            .put_value(
                crate::property::VALIDATION_RULE_PROP,
                Value::Text("[Num] >".into()),
            )
            .expect("put rule");
        maps.save(&mut db).expect("save properties");
        let err = db
            .add_rows("Test", &[vec![Value::Long(1), Value::Long(1)]])
            .expect_err("malformed rule");
        let msg = err.to_string();
        assert!(msg.contains("(Db="), "{}", msg);
        assert!(msg.contains("Table=Test;Column=Num"), "{}", msg);
        assert!(db.read_rows_with_ids("Test").expect("rows").is_empty());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A property update which changes `ValidationRule` / `ValidationText`
    /// reloads the table's validators (`TableImpl.propertiesUpdated`).
    #[test]
    fn validation_rule_property_update_takes_effect() {
        let (path, mut db) = create_test_db("validate_update_prop");
        db.create_table(&TableSpec::new(
            "Test",
            vec![ColumnSpec::new("Num", DataType::Long)
                .put_property(
                    crate::property::VALIDATION_RULE_PROP,
                    Value::Text("[Num] >= 0".into()),
                )
                .expect("rule")],
        ))
        .expect("create table");
        db.add_rows("Test", &[vec![Value::Long(5)]])
            .expect("initial rule accepts 5");
        let mut maps = db.get_table_properties_by_name("Test").expect("properties");
        maps.get_mut("Num")
            .put_value(
                crate::property::VALIDATION_RULE_PROP,
                Value::Text("[Num] >= 10".into()),
            )
            .expect("put rule");
        maps.get_mut("Num")
            .put_value(
                crate::property::VALIDATION_TEXT_PROP,
                Value::Text("Num must be >= 10".into()),
            )
            .expect("put text");
        maps.save(&mut db).expect("save properties");
        let err = db
            .add_rows("Test", &[vec![Value::Long(5)]])
            .expect_err("the new rule rejects 5");
        assert!(err.to_string().starts_with("Num must be >= 10 (Db="));
        db.add_rows("Test", &[vec![Value::Long(10)]])
            .expect("the new rule accepts 10");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // calculated columns
    // ------------------------------------------------------------------

    /// `resources/calc_fixture.bin`: the `Test` table definition page and its
    /// first data page of the Java generated `calc` table (V2016), written by
    /// `java-src/Gen.java`'s `calcfixture` scenario (see its javadoc for the
    /// regeneration command).
    const CALC_FIXTURE: &[u8] = include_bytes!("../resources/calc_fixture.bin");

    fn calc_spec() -> TableSpec {
        TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("A", DataType::Long),
                ColumnSpec::new("B", DataType::Double),
                ColumnSpec::new("S", DataType::Text).with_length(50),
                ColumnSpec::new("D", DataType::ShortDateTime),
                ColumnSpec::new("C", DataType::Long).set_calculated_info("[A]+1"),
                ColumnSpec::new("E", DataType::Double).set_calculated_info("[B]*2"),
                ColumnSpec::new("F", DataType::Text)
                    .with_length(30)
                    .set_calculated_info("[S] & \"x\""),
                ColumnSpec::new("G", DataType::Text)
                    .with_length(10)
                    .set_calculated_info("IIf([A]>0,\"pos\",\"neg\")"),
                ColumnSpec::new("H", DataType::Double).set_calculated_info("[C]+[E]"),
                ColumnSpec::new("J", DataType::Text)
                    .with_length(40)
                    .set_calculated_info("\"v=\" & [A]"),
                ColumnSpec::new("M", DataType::Memo).set_calculated_info("[S] & [S]"),
                ColumnSpec::new("N", DataType::Numeric).set_calculated_info("[A]*2"),
                ColumnSpec::new("K", DataType::ShortDateTime).set_calculated_info("[D]"),
                ColumnSpec::new("P", DataType::Boolean).set_calculated_info("[A]>0"),
            ],
        )
    }

    fn calc_ldt(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Value {
        Value::DateTime(crate::datenums::LocalDateTime::new(
            year, month, day, hour, minute, second, 0,
        ))
    }

    fn calc_row(a: Value, b: Value, s: Value, d: Value) -> Vec<Value> {
        let mut row = vec![Value::Null; 15];
        row[1] = a;
        row[2] = b;
        row[3] = s;
        row[4] = d;
        row
    }

    fn calc_rows() -> Vec<Vec<Value>> {
        vec![
            calc_row(
                Value::Long(5),
                Value::Double(2.5),
                Value::Text("abc".to_string()),
                calc_ldt(2020, 3, 15, 12, 30, 45),
            ),
            calc_row(
                Value::Long(-3),
                Value::Double(-1.5),
                Value::Text(String::new()),
                calc_ldt(2021, 1, 2, 3, 4, 5),
            ),
            calc_row(Value::Null, Value::Null, Value::Null, Value::Null),
            calc_row(
                Value::Long(0),
                Value::Double(0.25),
                Value::Text("z".to_string()),
                Value::Null,
            ),
            calc_row(
                Value::Long(100),
                Value::Double(-2.125),
                Value::Text("rowfive longer".to_string()),
                calc_ldt(2022, 6, 7, 8, 9, 10),
            ),
            calc_row(
                Value::Long(-1000),
                Value::Double(123.5),
                Value::Text("six".to_string()),
                calc_ldt(2023, 12, 31, 23, 59, 59),
            ),
        ]
    }

    fn create_calc_db(name: &str) -> (std::path::PathBuf, Database) {
        let path = test_tmp_dir().join(format!(
            "jackcess_calc_{}_{}.accdb",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2016, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    fn populate_calc_table(db: &mut Database) {
        db.create_table(&calc_spec()).expect("create calc table");
        for row in calc_rows() {
            db.add_rows("Test", &[row]).expect("add calc row");
        }
    }

    /// The table definition page and the row bytes (the calculated value
    /// wrappers included) match the bytes the Java library wrote, the result
    /// type and expression properties are the ones the Java builder writes, and
    /// reading the rows back unwraps the calculated values.
    #[test]
    fn calculated_definition_and_values_match_java_fixture() {
        let (path, mut db) = create_calc_db("defs");
        populate_calc_table(&mut db);
        db.flush().expect("flush");

        let tdef_page = db.table_lookup.get("TEST").expect("Test lookup").0;
        assert_eq!(tdef_page, 89, "Test tdef page");
        let bytes = std::fs::read(&path).expect("read database file");
        let page = |n: i32| &bytes[n as usize * 4096..(n as usize + 1) * 4096];
        assert_eq!(page(tdef_page), &CALC_FIXTURE[..4096], "table definition");
        // the data page follows the definition and its usage map page
        let data_page = (tdef_page..tdef_page + 32)
            .find(|p| {
                let p = *p;
                page(p)[0] == 0x01
                    && i32::from_le_bytes(page(p)[4..8].try_into().unwrap()) == tdef_page
            })
            .expect("data page");
        assert_eq!(data_page, tdef_page + 2, "data page number");
        assert_eq!(page(data_page), &CALC_FIXTURE[4096..], "data page bytes");

        {
            let table = db.tables.get(&tdef_page).expect("Test loaded");
            // the calculated length rules: fixed types 39, var types + 23
            let c = table.get_column("C").expect("C");
            assert!(c.calculated);
            assert_eq!(
                c.ext_flags & crate::column::CALCULATED_EXT_FLAG_MASK,
                0xC0,
                "calculated ext flags"
            );
            assert_eq!(c.data_type, DataType::Long);
            assert_eq!(c.column_length, crate::column::CALC_FIXED_FIELD_LEN);
            assert_eq!(c.column_length, 39);
            assert_eq!(table.get_column("E").unwrap().column_length, 39);
            assert_eq!(table.get_column("P").unwrap().column_length, 39);
            assert_eq!(table.get_column("F").unwrap().column_length, 53);
            assert_eq!(table.get_column("J").unwrap().column_length, 63);
            // a calculated MEMO writes the unused length as 0
            assert_eq!(table.get_column("M").unwrap().column_length, 0);
            assert_eq!(table.get_column("M").unwrap().data_type, DataType::Memo);
            assert_eq!(table.get_column("P").unwrap().data_type, DataType::Boolean);
        }

        // the column properties hold the expression and the real result type
        let maps = db.get_table_properties(tdef_page).expect("properties");
        let c_props = maps.get("C").expect("C property map");
        assert_eq!(
            c_props.get_value(crate::property::EXPRESSION_PROP),
            Some(&Value::Text("[A]+1".to_string()))
        );
        assert_eq!(
            c_props.get_value(crate::property::RESULT_TYPE_PROP),
            Some(&Value::Byte(0x04))
        );
        assert_eq!(
            maps.get("P")
                .unwrap()
                .get_value(crate::property::RESULT_TYPE_PROP),
            Some(&Value::Byte(0x01))
        );
        assert_eq!(
            maps.get("M")
                .unwrap()
                .get_value(crate::property::EXPRESSION_PROP),
            Some(&Value::Text("[S] & [S]".to_string()))
        );

        // reading the rows unwraps the calculated values
        let rows: Vec<Vec<Value>> = db
            .read_rows_with_ids("Test")
            .expect("rows")
            .into_iter()
            .filter_map(|(_, values)| values)
            .collect();
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0][1], Value::Long(5));
        assert_eq!(rows[0][5], Value::Long(6), "C = A+1");
        assert_eq!(rows[0][6], Value::Double(5.0), "E = B*2");
        assert_eq!(rows[0][7], Value::Text("abcx".to_string()), "F = S & x");
        assert_eq!(rows[0][8], Value::Text("pos".to_string()), "G = IIf(A>0)");
        assert_eq!(rows[0][9], Value::Double(11.0), "H = C+E");
        assert_eq!(rows[0][10], Value::Text("v=5".to_string()), "J");
        assert_eq!(rows[0][11], Value::Text("abcabc".to_string()), "M");
        assert_eq!(
            rows[0][12],
            Value::Decimal {
                unscaled: 10,
                scale: 0
            },
            "N = A*2 as NUMERIC"
        );
        // the calculated date value equals the date the row holds (a date is a
        // floating point value, so it is compared with the stored column value)
        assert_eq!(rows[0][13], rows[0][4], "K = D");
        assert_eq!(rows[0][14], Value::Bool(true), "P");

        assert_eq!(rows[1][5], Value::Long(-2));
        assert_eq!(rows[1][7], Value::Text("x".to_string()));
        assert_eq!(rows[1][8], Value::Text("neg".to_string()));
        assert_eq!(rows[1][9], Value::Double(-5.0));
        assert_eq!(rows[1][11], Value::Text(String::new()));
        assert_eq!(
            rows[1][12],
            Value::Decimal {
                unscaled: -6,
                scale: 0
            }
        );
        assert_eq!(rows[1][13], rows[1][4]);
        assert_eq!(rows[1][14], Value::Bool(false));

        // the all-null row: C/E/H/N/K/P are null (a null calculated boolean is
        // written as no data and stays null in the row), the concatenations are
        // empty
        assert_eq!(rows[2][5], Value::Null);
        assert_eq!(rows[2][6], Value::Null);
        assert_eq!(rows[2][7], Value::Text("x".to_string()));
        assert_eq!(rows[2][8], Value::Text("neg".to_string()));
        assert_eq!(rows[2][9], Value::Null);
        assert_eq!(rows[2][10], Value::Text("v=".to_string()));
        assert_eq!(rows[2][11], Value::Text(String::new()));
        assert_eq!(rows[2][12], Value::Null);
        assert_eq!(rows[2][13], Value::Null);
        assert_eq!(rows[2][14], Value::Null);

        assert_eq!(rows[3][5], Value::Long(1));
        assert_eq!(rows[3][7], Value::Text("zx".to_string()));
        assert_eq!(rows[3][8], Value::Text("neg".to_string()));
        assert_eq!(rows[3][9], Value::Double(1.5));
        assert_eq!(
            rows[3][12],
            Value::Decimal {
                unscaled: 0,
                scale: 0
            }
        );

        assert_eq!(rows[4][5], Value::Long(101));
        assert_eq!(rows[4][6], Value::Double(-4.25));
        assert_eq!(rows[4][7], Value::Text("rowfive longerx".to_string()));
        assert_eq!(rows[4][8], Value::Text("pos".to_string()));
        assert_eq!(rows[4][9], Value::Double(96.75));
        assert_eq!(rows[4][10], Value::Text("v=100".to_string()));
        assert_eq!(
            rows[4][11],
            Value::Text("rowfive longerrowfive longer".to_string())
        );
        assert_eq!(
            rows[4][12],
            Value::Decimal {
                unscaled: 200,
                scale: 0
            }
        );
        assert_eq!(rows[4][13], rows[4][4]);
        assert_eq!(rows[4][14], Value::Bool(true));

        assert_eq!(rows[5][5], Value::Long(-999));
        assert_eq!(rows[5][9], Value::Double(-752.0));
        assert_eq!(rows[5][10], Value::Text("v=-1000".to_string()));
        assert_eq!(
            rows[5][12],
            Value::Decimal {
                unscaled: -2000,
                scale: 0
            }
        );

        // the read/write round trip re-encodes every row (the calculated value
        // wrappers included) to the exact bytes the row holds
        let page_buffer = {
            let mut buffer = db.channel.create_page_buffer();
            db.channel
                .read_page(&mut buffer, data_page)
                .expect("read data page");
            buffer
        };
        let mut reencode_buffer = crate::bytebuf::ByteBuf::allocate(4096);
        // row 4's calculated MEMO value lives on a long value page, so writing
        // it again would allocate a new page (and point the row at it); the
        // other rows' calculated values are all inline
        for row_num in [0usize, 1, 2, 3, 5] {
            let start =
                find_row_start(&page_buffer, row_num, &db.format).expect("row start") as usize;
            let end = find_row_end(&page_buffer, row_num, &db.format).expect("row end");
            let raw = page_buffer
                .get_bytes_at(start, end - start)
                .expect("raw row");
            let values = {
                let table = db.tables.get(&tdef_page).expect("Test loaded");
                table
                    .read_row(&mut db.channel, &page_buffer, row_num)
                    .expect("read row")
            };
            reencode_buffer.set_limit(4096).expect("limit");
            reencode_buffer.set_position(0).expect("position");
            {
                let tz = db.tz;
                let table = db.tables.get_mut(&tdef_page).expect("Test loaded");
                table
                    .create_row(&mut db.channel, &values, &mut reencode_buffer, 0, &tz)
                    .expect("create row");
            }
            assert_eq!(
                reencode_buffer.remaining_bytes(),
                &raw[..],
                "row {} re-encodes identically",
                row_num
            );
        }

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Updates recompute every calculated column (in dependency order) even when
    /// the column is passed as `KEEP_VALUE`, and an all-keep update rewrites the
    /// exact same row bytes.
    #[test]
    fn calculated_columns_recompute_on_update() {
        let (path, mut db) = create_calc_db("update");
        db.create_table(&calc_spec()).expect("create calc table");
        db.add_rows(
            "Test",
            &[calc_row(
                Value::Long(5),
                Value::Double(2.5),
                Value::Text("abc".to_string()),
                calc_ldt(2020, 3, 15, 12, 30, 45),
            )],
        )
        .expect("add row");

        let (row_id, row) = db
            .read_rows_with_ids("Test")
            .expect("rows")
            .into_iter()
            .next()
            .expect("row");
        let row = row.expect("row values");
        assert_eq!(row[5], Value::Long(6));

        // change A, keep everything else (including the calculated columns)
        let mut update = vec![Value::Keep; 15];
        update[1] = Value::Long(10);
        db.update_row("Test", row_id, &update).expect("update A");
        let row = db
            .tables
            .get(&db.table_lookup.get("TEST").unwrap().0)
            .unwrap()
            .read_row_at(&mut db.channel, row_id)
            .expect("read back");
        assert_eq!(row[5], Value::Long(11), "C recomputed");
        assert_eq!(row[6], Value::Double(5.0), "E kept (B unchanged)");
        assert_eq!(row[9], Value::Double(16.0), "H recomputed from C and E");
        assert_eq!(row[14], Value::Bool(true), "P recomputed");

        // change B only: E and H follow
        let mut update = row.clone();
        update[2] = Value::Double(7.5);
        db.update_row("Test", row_id, &update).expect("update B");
        let row = db
            .tables
            .get(&db.table_lookup.get("TEST").unwrap().0)
            .unwrap()
            .read_row_at(&mut db.channel, row_id)
            .expect("read back");
        assert_eq!(row[6], Value::Double(15.0), "E recomputed");
        assert_eq!(row[9], Value::Double(26.0), "H recomputed");

        // an update which keeps every value writes the same row bytes
        let page = db.table_lookup.get("TEST").unwrap().0;
        let before = {
            let table = db.tables.get(&page).unwrap();
            let loc = table
                .position_at_row_data(&mut db.channel, row_id)
                .expect("locate row");
            loc.final_page.backing()[loc.row_start..loc.row_end].to_vec()
        };
        db.update_row("Test", row_id, &vec![Value::Keep; 15])
            .expect("all keep update");
        let after = {
            let table = db.tables.get(&page).unwrap();
            let loc = table
                .position_at_row_data(&mut db.channel, row_id)
                .expect("locate row");
            loc.final_page.backing()[loc.row_start..loc.row_end].to_vec()
        };
        assert_eq!(before, after, "all-keep update keeps the row bytes");

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A calculated column which references another one is evaluated after it,
    /// and a reference cycle is rejected like the Java `Cycle detected`.
    #[test]
    fn calculated_columns_are_sorted_by_dependency_and_cycles_are_rejected() {
        let (path, mut db) = create_calc_db("deps");
        let spec = TableSpec::new(
            "Test",
            vec![
                ColumnSpec::new("N", DataType::Long),
                // defined before the column it references, so a dependency
                // ordering is required
                ColumnSpec::new("X", DataType::Long).set_calculated_info("[Y]*2"),
                ColumnSpec::new("Y", DataType::Long).set_calculated_info("[N]+1"),
            ],
        );
        db.create_table(&spec).expect("create table");
        db.add_rows("Test", &[vec![Value::Long(4), Value::Null, Value::Null]])
            .expect("add row");
        let (_, row) = db
            .read_rows_with_ids("Test")
            .expect("rows")
            .into_iter()
            .next()
            .expect("row");
        let row = row.expect("row values");
        assert_eq!(row[2], Value::Long(5), "Y = N+1");
        assert_eq!(row[1], Value::Long(10), "X = Y*2");

        let cycle_spec = TableSpec::new(
            "Cycle",
            vec![
                ColumnSpec::new("X", DataType::Long).set_calculated_info("[Y]+1"),
                ColumnSpec::new("Y", DataType::Long).set_calculated_info("[X]+1"),
            ],
        );
        db.create_table(&cycle_spec).expect("create cycle table");
        let err = db
            .add_rows("Cycle", &[vec![Value::Null, Value::Null]])
            .expect_err("cycle rejected");
        assert!(
            err.to_string().contains("Cycle detected"),
            "unexpected error: {err}"
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Calculated columns need the V14 (Access 2010) format: the V2003 and
    /// V2007 builders reject them with the Java `ColumnBuilder.validate`
    /// message.
    #[test]
    fn calculated_columns_are_rejected_before_v2010() {
        for (format, name) in [
            (FileFormat::V2003, "VERSION_4"),
            (FileFormat::V2007, "VERSION_2007"),
        ] {
            let path = test_tmp_dir().join(format!(
                "jackcess_calc_reject_{}_{}.mdb",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            let mut db = Database::create(format, &path).expect("create database");
            let spec = TableSpec::new(
                "T",
                vec![ColumnSpec::new("C", DataType::Long).set_calculated_info("[N]+1")],
            );
            let err = db.create_table(&spec).expect_err("calculated column");
            let msg = err.to_string();
            assert!(
                msg.contains(&format!(
                    "Database format {name} does not support calculated type LONG(Column=C)"
                )),
                "unexpected error: {msg}"
            );
            drop(db);
            let _ = std::fs::remove_file(&path);
        }
    }

    /// A calculated column added to an existing table (`ColumnBuilder
    /// .addToTable`) gets its definition and properties, and the rows written
    /// after it are calculated by the table's (reloaded) calculated column
    /// state.
    #[test]
    fn calculated_column_can_be_added_to_an_existing_table() {
        let (path, mut db) = create_calc_db("addcol");
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("N", DataType::Long),
            ],
        );
        db.create_table(&spec).expect("create table");
        db.add_rows("T", &[vec![Value::Null, Value::Long(3)]])
            .expect("add row");

        let column = db
            .add_column(
                "T",
                ColumnSpec::new("K", DataType::Long).set_calculated_info("[N]*2"),
            )
            .expect("add calculated column");
        assert!(column.calculated);
        assert_eq!(column.column_length, 39);

        let page = db.table_lookup.get("T").unwrap().0;
        let maps = db.get_table_properties(page).expect("properties");
        let k_props = maps.get("K").expect("K property map");
        assert_eq!(
            k_props.get_value(crate::property::EXPRESSION_PROP),
            Some(&Value::Text("[N]*2".to_string()))
        );
        assert_eq!(
            k_props.get_value(crate::property::RESULT_TYPE_PROP),
            Some(&Value::Byte(0x04))
        );

        // a row added after the column exists gets the calculated value
        let tz = db.tz;
        let row_id = {
            let table = db.tables.get_mut(&page).unwrap();
            table
                .add_rows_with_ids(
                    &mut db.channel,
                    &[vec![Value::Null, Value::Long(7), Value::Null]],
                    &tz,
                )
                .expect("add row")
                .remove(0)
        };
        let row = db
            .tables
            .get(&page)
            .unwrap()
            .read_row_at(&mut db.channel, row_id)
            .expect("read back");
        assert_eq!(row[2], Value::Long(14), "K = N*2");

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `Table.getRowCount`, `Column.getSQLType`, `Column.getLengthInUnits` and
    /// `Column.getPrecision`/`getScale` on a loaded table.
    #[test]
    fn column_metadata_and_row_count() {
        let (path, mut db) = create_test_db("colmeta");
        let spec = TableSpec::new(
            "Meta",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(50),
                ColumnSpec::new("Qty", DataType::Numeric)
                    .with_precision(10)
                    .with_scale(2),
                ColumnSpec::new("Amount", DataType::Money),
                ColumnSpec::new("Note", DataType::Memo),
                ColumnSpec::new("Active", DataType::Boolean),
                ColumnSpec::new("Created", DataType::ShortDateTime),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        let page = db.create_table(&spec).expect("create table");

        let rows = vec![
            vec![
                Value::Null,
                Value::Text("one".to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Bool(true),
                Value::Null,
            ],
            vec![
                Value::Null,
                Value::Text("two".to_string()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Bool(false),
                Value::Null,
            ],
        ];
        db.add_rows("Meta", &rows).expect("add rows");

        db.load_table(page, "Meta", 0).expect("load table");
        let table = db.tables.get(&page).unwrap();
        assert_eq!(table.get_row_count(), 2);

        let id = table.get_column_meta_data("id").expect("ID metadata");
        assert_eq!(id.data_type, DataType::Long);
        assert_eq!(id.sql_type, Some(crate::datatype::sql_types::INTEGER));
        assert_eq!(id.length, 4);
        assert_eq!(id.length_in_units, 4);
        assert!(id.auto_number);

        let name = table.get_column_meta_data("NAME").expect("Name metadata");
        assert_eq!(name.data_type, DataType::Text);
        assert_eq!(name.sql_type, Some(crate::datatype::sql_types::VARCHAR));
        assert_eq!(name.length, 50);
        assert_eq!(name.length_in_units, 25, "text lengths are in characters");
        assert!(name.variable_length);

        let qty = table.get_column_meta_data("Qty").expect("Qty metadata");
        assert_eq!(qty.sql_type, Some(crate::datatype::sql_types::NUMERIC));
        assert_eq!((qty.precision, qty.scale), (10, 2));

        let amount = table
            .get_column_meta_data("Amount")
            .expect("Amount metadata");
        assert_eq!(amount.sql_type, Some(crate::datatype::sql_types::DECIMAL));
        assert_eq!((amount.precision, amount.scale), (19, 4), "MONEY defaults");

        let note = table.get_column_meta_data("Note").expect("Note metadata");
        assert_eq!(note.sql_type, Some(crate::datatype::sql_types::LONGVARCHAR));
        assert!(note.variable_length);

        let created = table
            .get_column_meta_data("Created")
            .expect("Created metadata");
        assert_eq!(
            created.sql_type,
            Some(crate::datatype::sql_types::TIMESTAMP)
        );

        // all metadata, in definition order
        let all = table.get_column_meta_data_all();
        assert_eq!(
            all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            vec!["ID", "Name", "Qty", "Amount", "Note", "Active", "Created"]
        );

        // primary key lookup
        let pk = table.get_primary_key_index().expect("primary key");
        assert_eq!(pk.name, "PrimaryKey");
        assert!(table.get_index("primarykey").is_ok());
        assert!(table.get_index("nope").is_err());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // GUID auto-number columns (`ColumnImpl.GuidAutoNumberGenerator`)
    // ------------------------------------------------------------------

    /// A GUID auto-number primary key plus a plain GUID column.
    fn create_guid_auto_db(name: &str) -> (std::path::PathBuf, Database) {
        let path =
            test_tmp_dir().join(format!("jackcess_guid_{}_{}.mdb", name, std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).expect("create database");
        db.now_ms = Some(1_700_000_000_000);
        let spec = TableSpec::new(
            "G",
            vec![
                ColumnSpec::new("ID", DataType::Guid).with_auto_number(true),
                ColumnSpec::new("Ref", DataType::Guid),
                ColumnSpec::new("Label", DataType::Text).with_length(50),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).expect("create table");
        (path, db)
    }

    fn guid_raw(value: &Value, what: &str) -> [u8; 16] {
        match value {
            Value::Uuid(bytes) => *bytes,
            other => panic!("{what} is not a GUID value: {other:?}"),
        }
    }

    fn guid_stored(literal: &str) -> [u8; 16] {
        let canonical = column::parse_guid(literal).expect("canonical GUID");
        column::guid_stored_bytes(&canonical, crate::bytebuf::ByteOrder::LittleEndian)
    }

    fn assert_guid_v4(raw: &[u8; 16], what: &str) {
        let canonical = column::guid_canonical_bytes(raw, crate::bytebuf::ByteOrder::LittleEndian);
        assert_eq!(canonical[6] >> 4, 4, "{what} is not version 4");
        assert_eq!((canonical[8] >> 6) & 0x03, 2, "{what} is not variant 2");
    }

    fn guid_rows(db: &mut Database) -> Vec<(RowId, Vec<Value>)> {
        db.read_rows_with_ids("G")
            .expect("read rows")
            .into_iter()
            .filter_map(|(row_id, values)| values.map(|values| (row_id, values)))
            .collect()
    }

    fn guid_label_row(db: &mut Database, label: &str) -> (RowId, Vec<Value>) {
        guid_rows(db)
            .into_iter()
            .find(|(_, values)| values[2] == Value::Text(label.to_string()))
            .unwrap_or_else(|| panic!("row {label} not found"))
    }

    /// `GuidAutoNumberGenerator.getNext`: a null / absent GUID is replaced by a
    /// fresh version 4 UUID; the values are unique and survive a reopen.
    #[test]
    fn guid_auto_number_generation_is_a_fresh_v4() {
        let (path, mut db) = create_guid_auto_db("gen");
        let rows: Vec<Vec<Value>> = (0..5)
            .map(|i| vec![Value::Null, Value::Null, Value::Text(format!("r{i}"))])
            .collect();
        db.add_rows("G", &rows).expect("add rows");

        let table = db.get_table("G").expect("table");
        assert!(table.columns[0].auto_number);
        assert_eq!(table.columns[0].data_type, DataType::Guid);
        assert!(!table.columns[1].auto_number);

        let stored: Vec<[u8; 16]> = guid_rows(&mut db)
            .into_iter()
            .map(|(_, values)| {
                let raw = guid_raw(&values[0], "ID");
                assert_guid_v4(&raw, "generated ID");
                raw
            })
            .collect();
        assert_eq!(stored.len(), 5);
        let unique: std::collections::HashSet<[u8; 16]> = stored.iter().copied().collect();
        assert_eq!(unique.len(), stored.len(), "generated IDs are not unique");

        db.flush().expect("flush");
        drop(db);
        let mut db = Database::open_path(&path).expect("reopen");
        let reopened: Vec<[u8; 16]> = guid_rows(&mut db)
            .into_iter()
            .map(|(_, values)| guid_raw(&values[0], "reopened ID"))
            .collect();
        assert_eq!(reopened, stored);
        assert!(db.get_table("G").expect("table").columns[0].auto_number);
        assert!(!db.is_allow_auto_number_insert());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `TableImpl.getInputAutoNumberRowValue` + `handleInsert`: with the policy
    /// off an explicit value is ignored and a fresh GUID is generated; with it
    /// on the explicit value (string or raw bytes) is stored; a null still
    /// generates; the per-table override wins; a bad value is rejected with the
    /// Java message and consumes nothing.
    #[test]
    fn guid_auto_number_policy_and_explicit_insert() {
        let (path, mut db) = create_guid_auto_db("policy");
        let literal = "00112233-4455-6677-8899-AABBCCDDEEFF";

        // the policy is off by default: the explicit value is ignored
        db.add_rows(
            "G",
            &[vec![
                Value::Text(literal.to_string()),
                Value::Null,
                Value::Text("off".to_string()),
            ]],
        )
        .expect("add with the policy off");
        let (_, values) = guid_label_row(&mut db, "off");
        let generated = guid_raw(&values[0], "ID");
        assert_guid_v4(&generated, "generated ID");
        assert_ne!(generated, guid_stored(literal));

        // policy on: the explicit value is stored, a null generates
        db.set_allow_auto_number_insert(true);
        db.add_rows(
            "G",
            &[
                vec![
                    Value::Text(format!("{{{literal}}}")),
                    Value::Text("aabbccdd-eeff-0011-2233-445566778899".to_string()),
                    Value::Text("on".to_string()),
                ],
                vec![Value::Null, Value::Null, Value::Text("on-null".to_string())],
            ],
        )
        .expect("add with the policy on");
        let (_, values) = guid_label_row(&mut db, "on");
        assert_eq!(guid_raw(&values[0], "ID"), guid_stored(literal));
        let (_, values) = guid_label_row(&mut db, "on-null");
        let generated = guid_raw(&values[0], "ID");
        assert_guid_v4(&generated, "generated ID");
        assert_ne!(generated, guid_stored(literal));

        // the per-table override wins over the database policy in both ways
        db.set_allow_auto_number_insert(false);
        db.set_table_allow_auto_number_insert("G", Some(true))
            .expect("table override on");
        db.add_rows(
            "G",
            &[vec![
                Value::Text("11111111-2222-3333-4444-555555555555".to_string()),
                Value::Null,
                Value::Text("table-on".to_string()),
            ]],
        )
        .expect("add with the table override on");
        let (_, values) = guid_label_row(&mut db, "table-on");
        assert_eq!(
            guid_raw(&values[0], "ID"),
            guid_stored("11111111-2222-3333-4444-555555555555")
        );

        db.set_allow_auto_number_insert(true);
        db.set_table_allow_auto_number_insert("G", Some(false))
            .expect("table override off");
        db.add_rows(
            "G",
            &[vec![
                Value::Text("22222222-3333-4444-5555-666666666666".to_string()),
                Value::Null,
                Value::Text("table-off".to_string()),
            ]],
        )
        .expect("add with the table override off");
        let (_, values) = guid_label_row(&mut db, "table-off");
        let generated = guid_raw(&values[0], "ID");
        assert_guid_v4(&generated, "generated ID");
        assert_ne!(
            generated,
            guid_stored("22222222-3333-4444-5555-666666666666")
        );

        // remove the override: the database policy (on) is back in charge
        db.set_table_allow_auto_number_insert("G", None)
            .expect("clear the table override");

        // a bad explicit GUID is rejected like `ColumnImpl.writeGUIDValue`
        db.set_allow_auto_number_insert(true);
        let before = guid_rows(&mut db).len();
        for (bad, text) in [
            (Value::Text("not-a-guid".to_string()), "not-a-guid"),
            (Value::Long(123), "123"),
            // the Java GUID pattern needs the hyphens in the canonical positions
            (
                Value::Text("00112233445566778899AABBCCDDEEFF".to_string()),
                "00112233445566778899AABBCCDDEEFF",
            ),
        ] {
            let err = db
                .add_rows(
                    "G",
                    &[vec![bad, Value::Null, Value::Text("bad".to_string())]],
                )
                .expect_err("a bad explicit GUID must be rejected");
            let message = err.to_string();
            assert!(
                message.contains(&format!("Invalid GUID: {text}")),
                "{message}"
            );
            assert!(message.contains("Table=G;Column=ID"), "{message}");
            assert_eq!(guid_rows(&mut db).len(), before);
        }
        // the rejected inserts consumed nothing: a generated value still works
        db.add_rows(
            "G",
            &[vec![
                Value::Null,
                Value::Null,
                Value::Text("after".to_string()),
            ]],
        )
        .expect("add after a rejection");
        let (_, values) = guid_label_row(&mut db, "after");
        assert_guid_v4(&guid_raw(&values[0], "ID"), "ID after a rejection");
        assert_eq!(guid_rows(&mut db).len(), before + 1);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `TableImpl.handleAutoNumbersForUpdate`: an update never regenerates a
    /// GUID auto number, `KEEP_VALUE` and an explicit null keep it, and an
    /// explicit value is only stored when the policy allows it.
    #[test]
    fn guid_auto_number_update_keeps_the_value() {
        let (path, mut db) = create_guid_auto_db("update");
        let literal = "00112233-4455-6677-8899-AABBCCDDEEFF";
        db.set_allow_auto_number_insert(true);
        db.add_rows(
            "G",
            &[
                vec![
                    Value::Text(literal.to_string()),
                    Value::Null,
                    Value::Text("row".to_string()),
                ],
                vec![Value::Null, Value::Null, Value::Text("other".to_string())],
            ],
        )
        .expect("add rows");
        let (row_id, _) = guid_label_row(&mut db, "row");
        let old = guid_stored(literal);

        // policy off: an explicit update is ignored
        db.set_allow_auto_number_insert(false);
        db.update_row(
            "G",
            row_id,
            &[
                Value::Text("44444444-5555-6666-7777-888888888888".to_string()),
                Value::Keep,
                Value::Keep,
            ],
        )
        .expect("update with the policy off");
        let (_, values) = guid_label_row(&mut db, "row");
        assert_eq!(guid_raw(&values[0], "ID"), old);
        // every column kept
        db.update_row("G", row_id, &[Value::Keep, Value::Keep, Value::Keep])
            .expect("keep every column");
        let (_, values) = guid_label_row(&mut db, "row");
        assert_eq!(guid_raw(&values[0], "ID"), old);

        // policy on: an explicit value replaces the old one
        db.set_allow_auto_number_insert(true);
        let updated = "55555555-6666-7777-8888-999999999999";
        db.update_row(
            "G",
            row_id,
            &[Value::Text(updated.to_string()), Value::Keep, Value::Keep],
        )
        .expect("update with the policy on");
        let (_, values) = guid_label_row(&mut db, "row");
        assert_eq!(guid_raw(&values[0], "ID"), guid_stored(updated));
        let new = guid_stored(updated);
        // KEEP_VALUE and an explicit null keep the current value
        db.update_row("G", row_id, &[Value::Keep, Value::Keep, Value::Keep])
            .expect("keep the new value");
        let (_, values) = guid_label_row(&mut db, "row");
        assert_eq!(guid_raw(&values[0], "ID"), new);
        db.update_row("G", row_id, &[Value::Null, Value::Keep, Value::Keep])
            .expect("explicit null keeps the value");
        let (_, values) = guid_label_row(&mut db, "row");
        assert_eq!(guid_raw(&values[0], "ID"), new);

        // a bad explicit update is rejected and changes nothing
        let err = db
            .update_row(
                "G",
                row_id,
                &[
                    Value::Text("bad-update".to_string()),
                    Value::Keep,
                    Value::Keep,
                ],
            )
            .expect_err("a bad explicit update must be rejected");
        assert!(err.to_string().contains("Invalid GUID: bad-update"));
        let (_, values) = guid_label_row(&mut db, "row");
        assert_eq!(guid_raw(&values[0], "ID"), new);

        // the updated value is in the file for the next open
        db.flush().expect("flush");
        drop(db);
        let mut db = Database::open_path(&path).expect("reopen");
        let (_, values) = guid_label_row(&mut db, "row");
        assert_eq!(guid_raw(&values[0], "ID"), new);

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The explicit GUID bytes are written with the same page layout as
    /// `ColumnImpl.writeGUIDValue` (the first three UUID components swapped for
    /// the little endian Jet4/ACE page), for a text value and for raw bytes.
    #[test]
    fn guid_auto_number_explicit_insert_bytes() {
        let (path, mut db) = create_guid_auto_db("bytes");
        db.set_allow_auto_number_insert(true);
        let auto_literal = "{00112233-4455-6677-8899-AABBCCDDEEFF}";
        let plain_literal = "AABBCCDD-EEFF-0011-2233-445566778899";
        let plain_raw = guid_stored(plain_literal);
        db.add_rows(
            "G",
            &[vec![
                Value::Text(auto_literal.to_string()),
                Value::Uuid(plain_raw),
                Value::Text("bytes".to_string()),
            ]],
        )
        .expect("add row");
        let (_, values) = guid_label_row(&mut db, "bytes");
        assert_eq!(guid_raw(&values[0], "auto GUID"), guid_stored(auto_literal));
        assert_eq!(guid_raw(&values[1], "plain GUID"), plain_raw);

        // a lowercase, unbraced string is the same value
        db.add_rows(
            "G",
            &[vec![
                Value::Text("deadbeef-dead-beef-dead-beefdeadbeef".to_string()),
                Value::Null,
                Value::Text("lower".to_string()),
            ]],
        )
        .expect("add lowercase row");
        let (_, values) = guid_label_row(&mut db, "lower");
        assert_eq!(
            guid_raw(&values[0], "auto GUID"),
            guid_stored("DEADBEEF-DEAD-BEEF-DEAD-BEEFDEADBEEF")
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// `TableMutator.validateAutoNumberColumn`: a table holds at most one auto
    /// number column of a non-complex type.
    #[test]
    fn guid_auto_number_table_rejects_two_guid_auto_columns() {
        let (path, mut db) = create_guid_auto_db("dupe");
        let spec = TableSpec::new(
            "Bad",
            vec![
                ColumnSpec::new("A", DataType::Guid).with_auto_number(true),
                ColumnSpec::new("B", DataType::Guid).with_auto_number(true),
            ],
        );
        let err = db
            .create_table(&spec)
            .expect_err("two GUID auto number columns must be rejected");
        assert!(
            err.to_string()
                .contains("Can have at most one AutoNumber column of type GUID per table"),
            "{err}"
        );
        // the same type restriction applies to LONG auto numbers
        let spec = TableSpec::new(
            "Bad2",
            vec![
                ColumnSpec::new("A", DataType::Long).with_auto_number(true),
                ColumnSpec::new("B", DataType::Long).with_auto_number(true),
            ],
        );
        let err = db
            .create_table(&spec)
            .expect_err("two LONG auto number columns must be rejected");
        assert!(
            err.to_string()
                .contains("Can have at most one AutoNumber column of type LONG per table"),
            "{err}"
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A GUID primary-key table large enough to split the index pages: an
    /// explicit GUID update/delete removes the old index entry, so the old key
    /// can be inserted again (the primitives are still the ones
    /// `TableImpl.handleAutoNumbersForUpdate` uses).
    #[test]
    fn guid_auto_number_primary_key_update_and_delete() {
        let (path, mut db) = create_guid_auto_db("largepk");
        db.set_allow_auto_number_insert(true);
        let mut rows = Vec::new();
        for i in 0u32..300 {
            let literal = format!("{:08X}-0000-0000-0000-{:012X}", i, i);
            rows.push(vec![
                Value::Text(literal),
                Value::Null,
                Value::Text(format!("row{i}")),
            ]);
        }
        db.add_rows("G", &rows).expect("add 300 rows");
        // update the ID of a middle row to a fresh value (policy on)
        let (row_id, values) = guid_label_row(&mut db, "row150");
        let _ = guid_raw(&values[0], "old");
        let new_literal = "FFFFFFFF-0000-0000-0000-00000000FFFF";
        db.update_row(
            "G",
            row_id,
            &[
                Value::Text(new_literal.to_string()),
                Value::Keep,
                Value::Keep,
            ],
        )
        .expect("update the ID");
        // re-inserting the old ID must now succeed (the old index entry is gone)
        let result = db.add_rows(
            "G",
            &[vec![
                Value::Text(format!("{:08X}-0000-0000-0000-{:012X}", 150u32, 150u32)),
                Value::Null,
                Value::Text("reuse-old".to_string()),
            ]],
        );
        assert!(
            result.is_ok(),
            "old GUID index entry was not removed: {result:?}"
        );
        // and deleting + reinserting another explicit row works
        let (row_id, values) = guid_label_row(&mut db, "row250");
        let _ = guid_raw(&values[0], "deleted");
        db.delete_row("G", row_id).expect("delete a row");
        db.add_rows(
            "G",
            &[vec![
                Value::Text(format!("{:08X}-0000-0000-0000-{:012X}", 250u32, 250u32)),
                Value::Null,
                Value::Text("reuse-deleted".to_string()),
            ]],
        )
        .expect("reinsert the deleted ID");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // ErrorHandler (com.healthmarketscience.jackcess.util.ErrorHandler)
    // ------------------------------------------------------------------

    /// A handler which records every callback and returns a fixed replacement.
    struct RecordingHandler {
        calls: Mutex<Vec<RecordedCall>>,
        replacement: Value,
        fail: bool,
    }

    #[derive(Debug, PartialEq)]
    struct RecordedCall {
        column: String,
        column_type: DataType,
        data: Option<Vec<u8>>,
        location: String,
        table: String,
        error: String,
    }

    impl RecordingHandler {
        fn new(replacement: Value) -> RecordingHandler {
            RecordingHandler {
                calls: Mutex::new(Vec::new()),
                replacement,
                fail: false,
            }
        }

        fn failing() -> RecordingHandler {
            RecordingHandler {
                calls: Mutex::new(Vec::new()),
                replacement: Value::Null,
                fail: true,
            }
        }

        fn calls(&self) -> Vec<RecordedCall> {
            self.calls.lock().unwrap().drain(..).collect()
        }
    }

    impl ErrorHandler for RecordingHandler {
        fn handle_row_error(
            &self,
            column: &Column,
            column_data: Option<&[u8]>,
            location: &dyn ErrorLocation,
            error: Error,
        ) -> Result<Value> {
            self.calls.lock().unwrap().push(RecordedCall {
                column: column.name.clone(),
                column_type: column.data_type,
                data: column_data.map(|d| d.to_vec()),
                location: location.to_string(),
                table: location.get_table().name.clone(),
                error: error.to_string(),
            });
            if self.fail {
                Err(Error::Jackcess("handler says no".to_string()))
            } else {
                Ok(self.replacement.clone())
            }
        }
    }

    /// Damages the variable length end offset of the second column (`Name`) of
    /// the given row so the column bytes reach past the row.  That is the same
    /// damage `tools/verify-errorhandler.ps1` applies to the real fixture, and
    /// it makes the row read throw [`Error::BufferUnderflow`].
    fn corrupt_text_row(db: &mut Database, row_id: RowId) {
        let mut loc = locate(db, row_id);
        let column_count = loc
            .final_page
            .get_u8_at(loc.row_start)
            .expect("row column count") as usize;
        let mask_size = column_count.div_ceil(8);
        let row_len = loc.row_end - loc.row_start;
        // Name is variable length table index 0: its end offset is the second
        // entry of the (num_var + 1) offset table before the null mask
        let offset_pos = loc.row_start + row_len - mask_size - 4 - 2;
        loc.final_page
            .put_i16_at(offset_pos, 0x7FFF)
            .expect("patch the variable length offset");
        db.channel.start_write();
        let written = write_data_page(&mut db.channel, &loc.final_page, loc.final_page_number);
        db.channel.finish_write().expect("finish write");
        written.expect("write page");
    }

    fn text_error_handler_db(name: &str) -> (std::path::PathBuf, Database, RowId) {
        let (path, mut db) = create_test_db(name);
        make_table(&mut db);
        add_rows(&mut db, 3);
        let rows = db.read_rows_with_ids("T").expect("rows");
        let row_id = rows[1].0;
        corrupt_text_row(&mut db, row_id);
        (path, db, row_id)
    }

    #[test]
    fn error_handler_default_rethrows_the_original_error() {
        let (path, mut db, _) = text_error_handler_db("default");
        let error = db
            .read_rows_with_ids("T")
            .expect_err("broken row must fail");
        // the error is propagated unchanged (Java's ErrorHandler.DEFAULT
        // rethrows the same exception instance)
        assert!(
            matches!(error, Error::BufferUnderflow(_)),
            "expected the buffer underflow to be rethrown, got {error:?}"
        );
        // the table API surfaces the same error
        let page = table_page(&db);
        let table = db.tables.get(&page).unwrap();
        assert!(table.read_all_rows(&mut db.channel).is_err());
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn database_error_handler_replaces_the_broken_column() {
        let (path, mut db, row_id) = text_error_handler_db("db_handler");
        let handler = Arc::new(RecordingHandler::new(Value::Text("BAD".to_string())));
        db.set_error_handler(Some(handler.clone()));

        let rows = db.read_rows_with_ids("T").expect("rows with handler");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1].0, row_id);
        let broken = rows[1].1.as_ref().expect("row values");
        assert_eq!(broken[1], Value::Text("BAD".to_string()));
        // the other columns of the row and the other rows are untouched
        assert_eq!(broken[2], Value::Double(1.5));
        assert_eq!(rows[0].1.as_ref().unwrap()[1], Value::Text("name0".into()));
        assert_eq!(rows[2].1.as_ref().unwrap()[1], Value::Text("name2".into()));

        let calls = handler.calls();
        assert_eq!(calls.len(), 1, "exactly one callback for the broken column");
        assert_eq!(calls[0].table, "T");
        assert_eq!(calls[0].column, "Name");
        assert_eq!(calls[0].column_type, DataType::Text);
        assert!(calls[0].data.is_none(), "the bytes could not be read");
        assert_eq!(
            calls[0].location,
            format!(
                "RowState[headerRowId={}:{},finalRowId={}:{}]",
                row_id.0, row_id.1, row_id.0, row_id.1
            )
        );
        assert!(
            calls[0].error.contains("buffer underflow"),
            "unexpected error text: {}",
            calls[0].error
        );

        // `Table.getErrorHandler` falls back to the database handler
        let page = table_page(&db);
        let table = db.tables.get(&page).unwrap();
        assert_eq!(
            table
                .get_error_handler()
                .handle_row_error(
                    &table.columns[1],
                    None,
                    &RowStateLocation::new(table, row_id, row_id),
                    Error::Jackcess("x".into())
                )
                .unwrap(),
            Value::Text("BAD".to_string())
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn table_and_cursor_error_handlers_override_outer_levels() {
        let (path, mut db, row_id) = text_error_handler_db("override");
        // the database handler is applied to tables loaded before the call
        let page = table_page(&db);
        db.set_error_handler(Some(Arc::new(RecordingHandler::new(Value::Text(
            "DB".to_string(),
        )))));
        assert_eq!(
            db.read_rows_with_ids("T").unwrap()[1].1.as_ref().unwrap()[1],
            Value::Text("DB".to_string())
        );

        // table handler wins over the database handler
        db.tables
            .get_mut(&page)
            .unwrap()
            .set_error_handler(Some(Arc::new(RecordingHandler::new(Value::Text(
                "TABLE".to_string(),
            )))));
        assert_eq!(
            db.read_rows_with_ids("T").unwrap()[1].1.as_ref().unwrap()[1],
            Value::Text("TABLE".to_string())
        );

        // cursor handler wins over the table handler
        let mut cursor = crate::cursor::TableScanCursor::new(&mut db, "T").expect("cursor");
        let cursor_handler = Arc::new(RecordingHandler::new(Value::Text("CURSOR".to_string())));
        cursor.set_error_handler(Some(cursor_handler.clone()));
        let mut seen = Vec::new();
        while let Some(values) = cursor.get_next_row(&mut db).expect("next row") {
            seen.push((cursor.get_current_row_id(), values));
        }
        assert_eq!(seen.len(), 3);
        let broken = seen.iter().find(|(id, _)| *id == row_id).unwrap();
        assert_eq!(broken.1[1], Value::Text("CURSOR".to_string()));
        // moving between rows must not read the rows (no extra callbacks)
        assert_eq!(cursor_handler.calls().len(), 1);

        // clearing the cursor handler returns to the table handler
        cursor.set_error_handler(None);
        cursor.before_first();
        let mut first = None;
        while let Some(values) = cursor.get_next_row(&mut db).expect("next row") {
            if cursor.get_current_row_id() == row_id {
                first = Some(values);
            }
        }
        assert_eq!(
            first.unwrap()[1],
            Value::Text("TABLE".to_string()),
            "without a cursor handler the table handler applies"
        );

        // clearing the table handler returns to the database handler
        db.tables.get_mut(&page).unwrap().set_error_handler(None);
        assert_eq!(
            db.read_rows_with_ids("T").unwrap()[1].1.as_ref().unwrap()[1],
            Value::Text("DB".to_string())
        );

        // clearing the database handler returns to the default (rethrow)
        db.set_error_handler(None);
        assert!(matches!(
            db.read_rows_with_ids("T"),
            Err(Error::BufferUnderflow(_))
        ));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn debug_error_handler_returns_null_and_records_the_java_message() {
        let (path, mut db, row_id) = text_error_handler_db("debug");
        let handler = Arc::new(DebugErrorHandler::new());
        db.set_error_handler(Some(handler.clone()));

        let rows = db.read_rows_with_ids("T").expect("rows with debug handler");
        assert_eq!(rows[1].1.as_ref().unwrap()[1], Value::Null);
        assert_eq!(rows.len(), 3);

        let messages = handler.messages();
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert!(
            message.starts_with("Failed reading column Column[name: (T) Name"),
            "unexpected message: {message}"
        );
        assert!(
            message.contains(&format!(
                "row RowState[headerRowId={}:{},finalRowId={}:{}], bytes null",
                row_id.0, row_id.1, row_id.0, row_id.1
            )),
            "unexpected message: {message}"
        );
        handler.clear_messages();
        assert!(handler.messages().is_empty());

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn handler_errors_propagate_to_the_caller() {
        let (path, mut db, _) = text_error_handler_db("throws");
        db.set_error_handler(Some(Arc::new(RecordingHandler::failing())));
        let error = db.read_rows_with_ids("T").expect_err("handler error");
        assert_eq!(error.to_string(), "handler says no");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // corrupt null mask (`NullMask.read` fails per column)
    // ------------------------------------------------------------------

    /// Damages the null mask declaration of the given row: overwrites the
    /// stored column count with `column_count` (little endian) and, when
    /// `mask_bytes` is given, writes them where a mask of that declared size
    /// starts (`rowEnd - maskSize`, the row end being an absolute page
    /// offset).
    ///
    /// Returns that mask position.  It is negative when the declared mask does
    /// not fit on the page (Java's `Buffer.position(int)` then throws), and in
    /// front of the row start when it fits on the page but not in the row (the
    /// row buffer mark is then invalidated and Java's `getRowColumnData`
    /// `reset()` throws `java.nio.InvalidMarkException`).
    fn corrupt_row_null_mask_with(
        db: &mut Database,
        tdef_page: i32,
        row_id: RowId,
        column_count: i64,
        mask_bytes: Option<&[u8]>,
    ) -> i64 {
        let mut loc = db
            .tables
            .get(&tdef_page)
            .expect("table loaded")
            .position_at_row_data(&mut db.channel, row_id)
            .expect("locate the row");
        let count_size = db.format.size_row_column_count;
        for i in 0..count_size {
            loc.final_page
                .put_u8_at(loc.row_start + i, ((column_count >> (8 * i)) & 0xFF) as u8)
                .expect("patch the row column count");
        }
        let mask_size = (column_count as usize).div_ceil(8);
        let mask_pos = loc.row_end as i64 - mask_size as i64;
        if let Some(mask_bytes) = mask_bytes {
            assert!(mask_pos >= 0, "the mask must fit on the page");
            let start = mask_pos as usize;
            for (i, byte) in mask_bytes.iter().enumerate() {
                loc.final_page
                    .put_u8_at(start + i, *byte)
                    .expect("patch the null mask");
            }
        }
        db.channel.start_write();
        let written = write_data_page(&mut db.channel, &loc.final_page, loc.final_page_number);
        db.channel.finish_write().expect("finish write");
        written.expect("write page");
        mask_pos
    }

    /// The `corrupt_nullmask_*` corruption: a 0xFFFF column count declares an
    /// 8192 byte mask, which cannot be positioned inside a 4096 byte page, so
    /// Java throws `IllegalArgumentException("newPosition < 0: (-4096 < 0)")`
    /// before any mask byte is read (every column is reported through the
    /// handler).
    fn corrupt_row_null_mask(db: &mut Database, tdef_page: i32, row_id: RowId) -> i64 {
        corrupt_row_null_mask_with(db, tdef_page, row_id, 0xFFFF, None)
    }

    /// A corrupt null mask is retried for every column: the default handler
    /// rethrows the `Buffer.position` error unchanged, a replacing handler
    /// gets one callback per column (with no raw bytes, because the mask fails
    /// before any column data is read) and the row is still returned.
    #[test]
    fn corrupt_null_mask_calls_the_handler_for_every_column() {
        let (path, mut db) = create_test_db("nullmask");
        make_table(&mut db);
        add_rows(&mut db, 3);
        let rows = db.read_rows_with_ids("T").expect("rows");
        let row_id = rows[1].0;
        let tdef_page = table_page(&db);
        let mask_position = corrupt_row_null_mask(&mut db, tdef_page, row_id);
        assert!(mask_position < 0, "the mask cannot fit on the page");
        let mask_message = format!("newPosition < 0: ({} < 0)", mask_position);

        // no handler: Java's `Buffer.position(int)` exception, untouched
        let error = db
            .read_rows_with_ids("T")
            .expect_err("corrupt mask must fail");
        assert!(
            matches!(&error, Error::IllegalArgument(message) if *message == mask_message),
            "unexpected error: {error:?}"
        );
        // the table API surfaces the same error
        {
            let table = db.tables.get(&tdef_page).unwrap();
            let error = table
                .read_all_rows(&mut db.channel)
                .expect_err("corrupt mask must fail");
            assert!(
                matches!(&error, Error::IllegalArgument(message) if *message == mask_message),
                "unexpected table error: {error:?}"
            );
        }

        // a replacing handler is called once per column, in column order
        let handler = Arc::new(RecordingHandler::new(Value::Text("BAD".to_string())));
        db.set_error_handler(Some(handler.clone()));
        let rows = db.read_rows_with_ids("T").expect("rows with handler");
        assert_eq!(rows.len(), 3, "the row is never skipped");
        let broken = rows[1].1.as_ref().expect("row values");
        assert_eq!(broken, &vec![Value::Text("BAD".to_string()); 3]);
        // the other rows are untouched
        assert_eq!(rows[0].1.as_ref().unwrap()[1], Value::Text("name0".into()));
        assert_eq!(rows[2].1.as_ref().unwrap()[1], Value::Text("name2".into()));

        let calls = handler.calls();
        assert_eq!(calls.len(), 3, "one callback per column: {calls:?}");
        let names: Vec<&str> = calls.iter().map(|call| call.column.as_str()).collect();
        assert_eq!(names, ["ID", "Name", "Value"]);
        for call in &calls {
            assert_eq!(call.table, "T");
            assert!(
                call.data.is_none(),
                "the mask failed before the bytes: {call:?}"
            );
            assert!(
                call.error.contains(&mask_message),
                "unexpected error text: {}",
                call.error
            );
            assert_eq!(
                call.location,
                format!(
                    "RowState[headerRowId={}:{},finalRowId={}:{}]",
                    row_id.0, row_id.1, row_id.0, row_id.1
                )
            );
        }

        // a throwing handler aborts the row read (the first column reports)
        db.set_error_handler(Some(Arc::new(RecordingHandler::failing())));
        let error = db.read_rows_with_ids("T").expect_err("handler error");
        assert_eq!(error.to_string(), "handler says no");

        // clearing the handler brings the mask error back (it is not cached)
        db.set_error_handler(None);
        assert!(matches!(
            db.read_rows_with_ids("T"),
            Err(Error::IllegalArgument(_))
        ));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A declared null mask which fits on the page but starts in front of the
    /// row start still reads, but it invalidates the row buffer mark.  Java's
    /// `getRowColumnData` starts with `rowBuffer.reset()`, so every non-null,
    /// non-boolean column then fails with `java.nio.InvalidMarkException`
    /// (the port's `ByteBuf::reset` error) while null and boolean columns are
    /// still returned; the row itself is never skipped.
    #[test]
    fn short_null_mask_fails_every_non_null_column() {
        let (path, mut db) = create_test_db("shortmask");
        make_table(&mut db);
        add_rows(&mut db, 3);
        let rows = db.read_rows_with_ids("T").expect("rows");
        let row_id = rows[0].0;
        let tdef_page = table_page(&db);
        // 32750 columns declare a 4094 byte mask, which starts at page offset
        // 2 (well in front of the row start).  Only ID and Name are marked
        // non-null (mask bit 0 and 1); Value is null.
        let mask_pos =
            corrupt_row_null_mask_with(&mut db, tdef_page, row_id, 32750, Some(&[0x03, 0x00]));
        assert_eq!(mask_pos, 2);
        assert!(
            mask_pos < locate(&mut db, row_id).row_start as i64,
            "the mask starts in front of the row"
        );

        // no handler: Java's InvalidMarkException surfaces for the first
        // non-null column
        let error = db.read_rows_with_ids("T").expect_err("the mark is invalid");
        assert_eq!(error.java_to_string(), "java.nio.InvalidMarkException");
        assert!(
            matches!(&error, Error::InvalidMark),
            "unexpected error: {error:?}"
        );

        // a replacing handler is called once per non-null, non-boolean column
        let handler = Arc::new(RecordingHandler::new(Value::Text("BAD".to_string())));
        db.set_error_handler(Some(handler.clone()));
        let rows = db.read_rows_with_ids("T").expect("rows with handler");
        assert_eq!(rows.len(), 3, "the row is never skipped");
        let broken = rows[0].1.as_ref().expect("row values");
        assert_eq!(
            broken,
            &vec![
                Value::Text("BAD".to_string()),
                Value::Text("BAD".to_string()),
                Value::Null
            ]
        );
        // the other rows are untouched
        assert_eq!(rows[1].1.as_ref().unwrap()[1], Value::Text("name1".into()));
        assert_eq!(rows[2].1.as_ref().unwrap()[1], Value::Text("name2".into()));

        let calls = handler.calls();
        assert_eq!(calls.len(), 2, "only the non-null columns: {calls:?}");
        let names: Vec<&str> = calls.iter().map(|call| call.column.as_str()).collect();
        assert_eq!(names, ["ID", "Name"]);
        for call in &calls {
            assert_eq!(call.table, "T");
            assert!(call.data.is_none(), "no bytes were located: {call:?}");
            assert!(
                call.error.contains("invalid mark"),
                "unexpected error text: {}",
                call.error
            );
            assert_eq!(
                call.location,
                format!(
                    "RowState[headerRowId={}:{},finalRowId={}:{}]",
                    row_id.0, row_id.1, row_id.0, row_id.1
                )
            );
        }

        // clearing the handler brings the reset failure back
        db.set_error_handler(None);
        assert!(matches!(
            db.read_rows_with_ids("T"),
            Err(Error::InvalidMark)
        ));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    // ------------------------------------------------------------------
    // corrupt null mask mutation paths (update / delete)
    // ------------------------------------------------------------------

    /// A table without indexes whose columns can be corrupted (`A` and `B` are
    /// variable length, `C` is fixed).
    fn make_unindexed_table(db: &mut Database, name: &str) {
        let spec = TableSpec::new(
            name,
            vec![
                ColumnSpec::new("A", DataType::Text).with_length(30),
                ColumnSpec::new("B", DataType::Text).with_length(30),
                ColumnSpec::new("C", DataType::Long),
            ],
        );
        db.create_table(&spec).expect("create table");
    }

    fn unindexed_rows(db: &mut Database, name: &str, n: usize) {
        let rows: Vec<Vec<Value>> = (0..n)
            .map(|i| {
                vec![
                    Value::Text(format!("a{i}")),
                    Value::Text(format!("b{i}")),
                    Value::Long(i as i32),
                ]
            })
            .collect();
        db.add_rows(name, &rows).expect("add rows");
    }

    fn table_page_of(db: &Database, name: &str) -> i32 {
        db.table_lookup
            .get(&name.to_uppercase())
            .expect("table lookup")
            .0
    }

    /// `updateRow` resolves every `KEEP_VALUE` by reading the old value (so a
    /// corrupt mask reports each read column through the handler) and survives
    /// the failed null mask read: the row is rewritten from the replacement
    /// values and its column count is repaired.
    #[test]
    fn corrupt_null_mask_update_reads_lazily_and_rewrites_the_row() {
        let (path, mut db) = create_test_db("nullmask_update");
        make_unindexed_table(&mut db, "U");
        unindexed_rows(&mut db, "U", 2);
        let row_id = db.read_rows_with_ids("U").expect("rows")[0].0;
        let tdef_page = table_page_of(&db, "U");
        let mask_position = corrupt_row_null_mask(&mut db, tdef_page, row_id);
        assert!(mask_position < 0);
        let mask_message = format!("newPosition < 0: ({} < 0)", mask_position);

        // a replacing handler: one callback per column (the reads are lazy, so
        // all three are read) and the update succeeds
        let handler = Arc::new(RecordingHandler::new(Value::Null));
        db.set_error_handler(Some(handler.clone()));
        db.update_row("U", row_id, &vec![Value::Keep; 3])
            .expect("the update reports the errors through the handler");
        let calls = handler.calls();
        let names: Vec<&str> = calls.iter().map(|call| call.column.as_str()).collect();
        assert_eq!(names, ["A", "B", "C"], "one lazy read per kept column");
        for call in &calls {
            assert!(call.error.contains(&mask_message), "{}", call.error);
        }

        // the row was rewritten with the replacement values and a valid column
        // count, so it reads back without the handler
        db.set_error_handler(None);
        let rows = db.read_rows_with_ids("U").expect("rows after the update");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].1.as_ref().unwrap(), &vec![Value::Null; 3]);
        assert_eq!(
            rows[1].1.as_ref().unwrap(),
            &vec![
                Value::Text("a1".to_string()),
                Value::Text("b1".to_string()),
                Value::Long(1)
            ]
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The failed raw value read of `readRawRowValue` is swallowed: a corrupt
    /// mark (the short mask) does not stop the update at the first kept
    /// variable length column, every kept column is still read through the
    /// handler, and only the `rowBuffer.reset()` before the row write fails
    /// (after the row bytes were built; with no index nothing was written).
    #[test]
    fn corrupt_null_mask_update_swallows_the_failed_raw_read() {
        let (path, mut db) = create_test_db("nullmask_raw");
        make_unindexed_table(&mut db, "V");
        unindexed_rows(&mut db, "V", 2);
        let row_id = db.read_rows_with_ids("V").expect("rows")[0].0;
        let tdef_page = table_page_of(&db, "V");
        // 32750 columns declare a 4094 byte mask at page offset 2: the mask
        // reads, but it invalidates the row buffer mark
        let mask_position =
            corrupt_row_null_mask_with(&mut db, tdef_page, row_id, 32750, Some(&[0x07, 0x00]));
        assert_eq!(mask_position, 2);
        let before = std::fs::read(&path).expect("read the database");

        let handler = Arc::new(RecordingHandler::new(Value::Null));
        db.set_error_handler(Some(handler.clone()));
        let error = db
            .update_row("V", row_id, &vec![Value::Keep; 3])
            .expect_err("the row buffer reset fails");
        assert!(
            matches!(error, Error::InvalidMark),
            "unexpected error: {error:?}"
        );
        let calls = handler.calls();
        let names: Vec<&str> = calls.iter().map(|call| call.column.as_str()).collect();
        assert_eq!(names, ["A", "B", "C"], "the raw read failure is swallowed");
        for call in &calls {
            assert!(call.error.contains("invalid mark"), "{}", call.error);
        }

        // the row (and the table definition) is untouched: the failure happens
        // before the new row bytes are written and there are no index changes
        drop(db);
        let after = std::fs::read(&path).expect("read the database again");
        assert_eq!(after, before, "the failed update wrote to the database");
        let _ = std::fs::remove_file(&path);
    }

    /// `deleteRow` decodes only `_indexColumns`: a corrupt mask reports one
    /// callback per index column, and a table without indexes deletes the row
    /// without reading (or reporting) anything.
    #[test]
    fn corrupt_null_mask_delete_decodes_only_the_index_columns() {
        let (path, mut db) = create_test_db("nullmask_delete");
        make_table(&mut db);
        add_rows(&mut db, 2);
        let row_id = db.read_rows_with_ids("T").expect("rows")[0].0;
        let tdef_page = table_page(&db);
        assert!(corrupt_row_null_mask(&mut db, tdef_page, row_id) < 0);
        let handler = Arc::new(RecordingHandler::new(Value::Null));
        db.set_error_handler(Some(handler.clone()));
        db.delete_row("T", row_id).expect("the delete succeeds");
        let calls = handler.calls();
        let names: Vec<&str> = calls.iter().map(|call| call.column.as_str()).collect();
        assert_eq!(names, ["ID"], "only the primary key column is decoded");
        db.set_error_handler(None);
        assert_eq!(
            db.read_rows_with_ids("T")
                .expect("rows after the delete")
                .into_iter()
                .filter_map(|(_, row)| row)
                .count(),
            1
        );
        drop(db);
        let _ = std::fs::remove_file(&path);

        // a table without an index: the delete never reads the row
        let (path, mut db) = create_test_db("nullmask_delete_unindexed");
        make_unindexed_table(&mut db, "U");
        unindexed_rows(&mut db, "U", 2);
        let row_id = db.read_rows_with_ids("U").expect("rows")[0].0;
        let tdef_page = table_page_of(&db, "U");
        assert!(corrupt_row_null_mask(&mut db, tdef_page, row_id) < 0);
        let handler = Arc::new(RecordingHandler::new(Value::Null));
        db.set_error_handler(Some(handler.clone()));
        db.delete_row("U", row_id).expect("the delete succeeds");
        assert!(
            handler.calls().is_empty(),
            "a delete without indexes decodes nothing"
        );
        db.set_error_handler(None);
        assert_eq!(
            db.read_rows_with_ids("U")
                .expect("rows after the delete")
                .into_iter()
                .filter_map(|(_, row)| row)
                .count(),
            1
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// Without a handler the update and the delete propagate the error of the
    /// first column read (and a delete without indexes does not read at all, so
    /// it cannot fail).
    #[test]
    fn corrupt_null_mask_update_and_delete_propagate_without_a_handler() {
        // the unreadable mask: `NullMask.read`'s Buffer.position error
        let (path, mut db) = create_test_db("nullmask_propagate");
        make_table(&mut db);
        add_rows(&mut db, 2);
        let row_id = db.read_rows_with_ids("T").expect("rows")[0].0;
        let tdef_page = table_page(&db);
        let mask_position = corrupt_row_null_mask(&mut db, tdef_page, row_id);
        let mask_message = format!("newPosition < 0: ({} < 0)", mask_position);
        let error = db
            .update_row("T", row_id, &vec![Value::Keep; 3])
            .expect_err("the update propagates the mask error");
        assert!(
            matches!(&error, Error::IllegalArgument(message) if *message == mask_message),
            "unexpected update error: {error:?}"
        );
        let error = db
            .delete_row("T", row_id)
            .expect_err("the delete propagates the mask error");
        assert!(
            matches!(&error, Error::IllegalArgument(message) if *message == mask_message),
            "unexpected delete error: {error:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);

        // the short mask invalidates the mark: the update reaches the in place
        // write and the delete reaches the first index column read
        let (path, mut db) = create_test_db("nullmask_propagate_mark");
        make_table(&mut db);
        add_rows(&mut db, 2);
        let row_id = db.read_rows_with_ids("T").expect("rows")[0].0;
        let tdef_page = table_page(&db);
        assert_eq!(
            corrupt_row_null_mask_with(&mut db, tdef_page, row_id, 32750, Some(&[0x07, 0x00])),
            2
        );
        let error = db
            .update_row("T", row_id, &vec![Value::Keep; 3])
            .expect_err("the row buffer reset fails");
        assert!(
            matches!(error, Error::InvalidMark),
            "unexpected error: {error:?}"
        );
        let error = db
            .delete_row("T", row_id)
            .expect_err("the index column read fails");
        assert!(
            matches!(error, Error::InvalidMark),
            "unexpected error: {error:?}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// files: one callback per column, no raw bytes, the error Java's
    /// `NullMask.read` throws and the row values Java's trace prints (see
    /// `tools/verify-errorhandler.ps1`).  The read leaves the file unchanged.
    #[test]
    fn corrupt_null_mask_on_the_real_fixtures_matches_the_java_trace() {
        use std::path::PathBuf;

        struct Fixture {
            version: &'static str,
            file: &'static str,
            table: &'static str,
            tag: &'static str,
            row_id: RowId,
            columns: &'static [(&'static str, DataType)],
            row_count: usize,
        }

        const COMMON_COLUMNS: &[(&str, DataType)] = &[
            ("A", DataType::Text),
            ("B", DataType::Text),
            ("C", DataType::Byte),
            ("D", DataType::Int),
            ("E", DataType::Long),
            ("F", DataType::Double),
            ("G", DataType::ShortDateTime),
            ("H", DataType::Money),
            ("I", DataType::Boolean),
        ];
        const FIXED_NUMERIC_COLUMNS: &[(&str, DataType)] = &[
            ("col1", DataType::Memo),
            ("col2", DataType::Numeric),
            ("col3", DataType::Numeric),
            ("col4", DataType::Numeric),
            ("col5", DataType::Numeric),
            ("col6", DataType::Numeric),
            ("col7", DataType::Numeric),
        ];

        let fixtures = [
            Fixture {
                version: "V2003",
                file: "common1V2003.mdb",
                table: "Table1",
                tag: "common2003",
                row_id: (27, 0),
                columns: COMMON_COLUMNS,
                row_count: 2,
            },
            Fixture {
                version: "V2003",
                file: "fixedNumericV2003.mdb",
                table: "test",
                tag: "fixed2003",
                row_id: (28, 0),
                columns: FIXED_NUMERIC_COLUMNS,
                row_count: 1,
            },
            Fixture {
                version: "V2007",
                file: "common1V2007.accdb",
                table: "Table1",
                tag: "common2007",
                row_id: (64, 0),
                columns: COMMON_COLUMNS,
                row_count: 2,
            },
        ];

        for fixture in fixtures {
            let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("external/jackcess-test-resources/data")
                .join(fixture.version)
                .join(fixture.file);
            if !source.exists() {
                eprintln!(
                    "skipping corrupt null mask fixture test: {} is missing",
                    source.display()
                );
                continue;
            }
            let copy: PathBuf = test_tmp_dir().join(format!(
                "jackcess_nullmask_{}_{}.mdb",
                fixture.tag,
                std::process::id()
            ));
            let _ = std::fs::remove_file(&copy);
            std::fs::copy(&source, &copy).expect("copy the fixture");

            // prepare the corruption exactly like the Java reference does
            {
                let mut db = Database::open_path(&copy).expect("open the fixture copy");
                let rows = db.read_rows_with_ids(fixture.table).expect("fixture rows");
                assert_eq!(rows.len(), fixture.row_count);
                assert_eq!(rows[0].0, fixture.row_id, "first row of the fixture");
                let tdef_page = db
                    .table_lookup
                    .get(&fixture.table.to_uppercase())
                    .expect("table lookup")
                    .0;
                // row 0 of its data page ends at the page end (4096), so the
                // 8192 byte mask starts 4096 bytes before the page
                assert_eq!(
                    corrupt_row_null_mask(&mut db, tdef_page, fixture.row_id),
                    -4096,
                    "{}",
                    fixture.tag
                );
            }
            let prepared = std::fs::read(&copy).expect("read the prepared fixture");

            // no handler: the exact Java exception text
            {
                let mut db = Database::open_path(&copy).expect("open the prepared fixture");
                let error = db
                    .read_rows_with_ids(fixture.table)
                    .expect_err("a corrupt null mask must fail without a handler");
                assert!(
                    matches!(&error, Error::IllegalArgument(message)
                        if message == "newPosition < 0: (-4096 < 0)"),
                    "{}: unexpected error: {error:?}",
                    fixture.tag
                );
            }

            // a replacing handler: one callback per column, all replaced
            let mut db = Database::open_path(&copy).expect("open the prepared fixture");
            let handler = Arc::new(RecordingHandler::new(Value::Text("BAD".to_string())));
            db.set_error_handler(Some(handler.clone()));
            let rows = db
                .read_rows_with_ids(fixture.table)
                .expect("prepared fixture rows");
            assert_eq!(rows.len(), fixture.row_count, "{}", fixture.tag);
            let damaged = rows
                .iter()
                .find(|(id, _)| *id == fixture.row_id)
                .expect("damaged row");
            let values = damaged.1.as_ref().expect("damaged row values");
            assert_eq!(
                values,
                &vec![Value::Text("BAD".to_string()); fixture.columns.len()],
                "{}",
                fixture.tag
            );
            // the other row is read normally (the damage is per row)
            for (id, other) in &rows {
                if *id != fixture.row_id {
                    let other = other.as_ref().expect("intact row");
                    assert_eq!(other.len(), fixture.columns.len(), "{}", fixture.tag);
                    assert_ne!(
                        other,
                        &vec![Value::Text("BAD".to_string()); fixture.columns.len()],
                        "{}: the other row must not be replaced",
                        fixture.tag
                    );
                }
            }

            let calls = handler.calls();
            assert_eq!(
                calls.len(),
                fixture.columns.len(),
                "{}: one callback per column: {calls:?}",
                fixture.tag
            );
            for (call, (name, data_type)) in calls.iter().zip(fixture.columns) {
                assert_eq!(call.table, fixture.table, "{}", fixture.tag);
                assert_eq!(call.column, *name, "{}", fixture.tag);
                assert_eq!(call.column_type, *data_type, "{}", fixture.tag);
                assert!(call.data.is_none(), "{}", fixture.tag);
                assert!(
                    call.error.contains("newPosition < 0: (-4096 < 0)"),
                    "{}: unexpected error text: {}",
                    fixture.tag,
                    call.error
                );
                assert_eq!(
                    call.location,
                    format!(
                        "RowState[headerRowId={}:{},finalRowId={}:{}]",
                        fixture.row_id.0, fixture.row_id.1, fixture.row_id.0, fixture.row_id.1
                    ),
                    "{}",
                    fixture.tag
                );
            }
            drop(db);

            // reading the corrupted file never writes to it
            let after = std::fs::read(&copy).expect("read the prepared fixture again");
            assert_eq!(after, prepared, "{}: the file changed", fixture.tag);
            let _ = std::fs::remove_file(&copy);
        }
    }

    /// The `corrupt_shortmask_v2003` fixture the Java reference builds from
    /// `common1V2003.mdb` (Table1): the declared 4094 byte mask starts at page
    /// offset 2, in front of the row, so the row buffer mark is invalidated.
    /// The 7 non-null, non-boolean columns A..G are reported with
    /// `java.nio.InvalidMarkException` (the port's `IllegalState("invalid
    /// mark")`), H (null) and I (BOOLEAN) keep their mask derived values and
    /// the row is still returned.  The file is left unchanged.
    #[test]
    fn short_null_mask_on_the_real_fixture_matches_the_java_trace() {
        let source = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external/jackcess-test-resources/data/V2003/common1V2003.mdb");
        if !source.exists() {
            eprintln!(
                "skipping short null mask fixture test: {} is missing",
                source.display()
            );
            return;
        }
        let copy = test_tmp_dir().join(format!("jackcess_shortmask_{}.mdb", std::process::id()));
        let _ = std::fs::remove_file(&copy);
        std::fs::copy(&source, &copy).expect("copy the fixture");
        let row_id = (27, 0);
        {
            let mut db = Database::open_path(&copy).expect("open the fixture copy");
            let rows = db.read_rows_with_ids("Table1").expect("fixture rows");
            assert_eq!(rows[0].0, row_id);
            let tdef_page = db.table_lookup.get("TABLE1").expect("table lookup").0;
            // 32750 columns -> 4094 mask bytes -> the mask starts at page
            // offset 2; 0x7F marks A..G non-null, H is null and I is false
            let mask_pos =
                corrupt_row_null_mask_with(&mut db, tdef_page, row_id, 32750, Some(&[0x7F, 0x00]));
            assert_eq!(mask_pos, 2);
        }
        let prepared = std::fs::read(&copy).expect("read the prepared fixture");

        // no handler: the reset failure surfaces
        {
            let mut db = Database::open_path(&copy).expect("open the prepared fixture");
            let error = db
                .read_rows_with_ids("Table1")
                .expect_err("the row mark is invalid");
            assert_eq!(error.java_to_string(), "java.nio.InvalidMarkException");
            assert!(
                matches!(&error, Error::InvalidMark),
                "unexpected error: {error:?}"
            );
        }

        let mut db = Database::open_path(&copy).expect("open the prepared fixture");
        let handler = Arc::new(RecordingHandler::new(Value::Null));
        db.set_error_handler(Some(handler.clone()));
        let rows = db.read_rows_with_ids("Table1").expect("rows with handler");
        assert_eq!(rows.len(), 2, "the row is never skipped");
        let damaged = rows
            .iter()
            .find(|(id, _)| *id == row_id)
            .expect("damaged row");
        let values = damaged.1.as_ref().expect("damaged row values");
        assert_eq!(
            values,
            &vec![
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Bool(false),
            ]
        );
        let calls = handler.calls();
        let names: Vec<&str> = calls.iter().map(|call| call.column.as_str()).collect();
        assert_eq!(names, ["A", "B", "C", "D", "E", "F", "G"]);
        for call in &calls {
            assert_eq!(call.table, "Table1");
            assert!(call.data.is_none(), "no bytes were located: {call:?}");
            assert!(
                call.error.contains("invalid mark"),
                "unexpected error text: {}",
                call.error
            );
            assert_eq!(call.location, "RowState[headerRowId=27:0,finalRowId=27:0]");
        }
        // the other row is read normally
        let other = rows
            .iter()
            .find(|(id, _)| *id != row_id)
            .expect("intact row")
            .1
            .as_ref()
            .expect("intact row values");
        assert_eq!(other[0], Value::Text("a".into()));
        assert_eq!(other[1], Value::Text("b".into()));
        drop(db);

        // reading the corrupted file never writes to it
        let after = std::fs::read(&copy).expect("read the prepared fixture again");
        assert_eq!(after, prepared, "the file changed");
        let _ = std::fs::remove_file(&copy);
    }

    #[test]
    fn unsupported_columns_are_read_as_raw_data_and_never_call_the_handler() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external/jackcess-test-resources/data/V2007/unsupportedFieldsV2007.accdb");
        if !path.exists() {
            eprintln!(
                "skipping unsupported column test: {} is missing",
                path.display()
            );
            return;
        }
        let mut db = Database::open_path(&path).expect("open fixture");
        let handler = Arc::new(RecordingHandler::new(Value::Null));
        db.set_error_handler(Some(handler.clone()));
        let rows = db
            .read_rows_with_ids("Test")
            .expect("unsupported columns read");
        assert_eq!(rows.len(), 3);
        let row0 = rows[0].1.as_ref().unwrap();
        assert_eq!(row0[0], Value::Long(1));
        // `UnsupportedColumnImpl.read` returns the raw bytes (not a handler
        // replacement), including the compressed unicode header
        assert_eq!(
            row0[1],
            Value::Bytes(vec![
                0xFF, 0xFE, 0x73, 0x6F, 0x6D, 0x65, 0x64, 0x61, 0x74, 0x61
            ])
        );
        assert_eq!(row0[2], Value::Bytes(vec![0x37, 0x00, 0x00, 0x00]));
        assert_eq!(row0[3], Value::Long(1));
        assert!(
            handler.calls().is_empty(),
            "unsupported columns must not call the handler"
        );
    }

    #[test]
    fn table_level_errors_are_not_handled() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external/jackcess-test-resources/data/V2007/linkedOdbcV2007.accdb");
        if !path.exists() {
            eprintln!(
                "skipping table level error test: {} is missing",
                path.display()
            );
            return;
        }
        let mut db = Database::open_path(&path).expect("open fixture");
        let handler = Arc::new(RecordingHandler::new(Value::Null));
        db.set_error_handler(Some(handler.clone()));
        let error = db
            .read_rows_with_ids("Ordrar")
            .expect_err("a table definition has no rows");
        assert!(error
            .to_string()
            .contains("TableDefinition has no data access"));
        assert!(handler.calls().is_empty());
    }

    /// The read-side table getters which were added for API surface parity:
    /// name/flags/counts, index lists, the table definition page, the
    /// approximate owned page count and the cursor's table name.
    #[test]
    fn read_side_table_getters() {
        let (path, mut db) = create_test_db("surface");
        make_table(&mut db);
        add_rows(&mut db, 3);
        let page = table_page(&db);

        let table = db.get_table("T").unwrap();
        assert_eq!(table.get_name(), "T");
        assert!(!table.is_system());
        assert!(!table.is_hidden());
        assert!(table.check_data_access().is_ok());
        assert_eq!(table.get_column_count(), 3);
        assert!(table.get_max_column_count() >= 3);
        assert!(table.has_column("name"));
        assert!(table.has_column("NAME"));
        assert!(!table.has_column("missing"));
        assert_eq!(table.get_table_def_page_number(), page);
        assert_eq!(table.get_row_count(), 3);
        assert!(table.get_created_date().is_some());
        assert_eq!(table.get_indexes().len(), 1);
        assert!(table.get_index("primarykey").is_ok());
        assert!(table.get_primary_key_index().is_ok());
        assert_eq!(table.get_logical_index_count(), 1);
        assert_eq!(table.get_index_datas().len(), 1);
        assert!(!table.is_definition_only());
        // one page for the table definition plus the owned data and index pages
        let expected_owned = table
            .owned_pages
            .as_ref()
            .map(|map| map.page_count())
            .unwrap_or(0) as i32
            + 1
            + table
                .column_umaps
                .values()
                .map(|(owned, _)| owned.page_count() as i32)
                .sum::<i32>()
            + table
                .index_datas
                .iter()
                .map(|data| {
                    data.owned_pages
                        .as_ref()
                        .map(|map| map.page_count())
                        .unwrap_or(0) as i32
                })
                .sum::<i32>();
        assert_eq!(table.get_approximate_owned_page_count(), expected_owned);
        assert!(expected_owned >= 2);
        // the added columns have no version history and are not append-only
        let columns = table.get_column_meta_data_all();
        assert_eq!(columns.len(), 3);
        assert!(columns
            .iter()
            .all(|column| column.version_history_column.is_none()));
        assert!(columns.iter().all(|column| !column.append_only));
        assert!(columns[1].length_in_units > 0);

        // the cursor exposes its owning table name (`Cursor.getTable().getName`)
        // and its id (`Cursor.getId`, which the savepoint id also reports)
        let cursor = crate::cursor::TableScanCursor::new(&mut db, "T").expect("cursor");
        assert_eq!(Cursor::table_name(&cursor), "T");
        let other_cursor = crate::cursor::TableScanCursor::new(&mut db, "T").expect("cursor");
        assert_eq!(Cursor::get_id(&cursor), Cursor::get_id(&other_cursor));
        assert_eq!(
            cursor.get_savepoint().get_cursor_id(),
            Cursor::get_id(&cursor)
        );

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// A `DATE`-mode row write goes through the database zone: a local time
    /// inside the New York spring-forward gap is stored as the wall clock time
    /// Java's `atZone(zone)` moves it to (03:30), and its index key uses the
    /// same double, so a lookup with the original `LocalDateTime` finds it.
    #[test]
    fn date_mode_rows_use_the_named_zone_for_values_and_index_keys() {
        let (path, mut db) = create_test_db("zoned_date");
        db.set_zone_id("America/New_York").unwrap();
        assert_eq!(db.get_zone_id(), "America/New_York");
        db.set_date_time_type(crate::database::DateTimeType::Date);
        let spec = TableSpec::new(
            "DT",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("D", DataType::ShortDateTime),
                ColumnSpec::new("D3", DataType::ShortDateTime)
                    .put_property(
                        crate::property::DEFAULT_VALUE_PROP,
                        crate::value::Value::Text("#2020-03-15 12:30:45#".to_string()),
                    )
                    .unwrap(),
            ],
        )
        .with_index(IndexSpec::new("DIndex", vec![("D".to_string(), true)]));
        let page = db.create_table(&spec).unwrap();
        let gap = crate::datenums::LocalDateTime::new(2020, 3, 8, 2, 30, 0, 0);
        db.add_rows(
            "DT",
            &[vec![Value::Long(1), Value::DateTime(gap), Value::Null]],
        )
        .unwrap();
        db.flush().unwrap();

        // the stored value is the 03:30 wall clock double Java writes
        let rows = db.read_table_rows(page).unwrap();
        let row = rows[0].clone().unwrap();
        let expected = crate::datenums::to_date_double(crate::datenums::LocalDateTime::new(
            2020, 3, 8, 3, 30, 0, 0,
        ));
        match row[1] {
            Value::DateBits(bits) => assert_eq!(bits.to_bits(), expected.to_bits()),
            ref other => panic!("expected a DATE value, got {other:?}"),
        }
        // and the date default (a wall clock time) took the zone too
        match row[2] {
            Value::DateBits(bits) => assert_eq!(
                bits.to_bits(),
                crate::datenums::to_date_double(crate::datenums::LocalDateTime::new(
                    2020, 3, 15, 12, 30, 45, 0
                ))
                .to_bits()
            ),
            ref other => panic!("expected the default DATE value, got {other:?}"),
        }

        // the indexed lookup finds the row with the original wall clock time
        db.find_table("DT").unwrap();
        let mut range = crate::cursor::IndexRange::new();
        let mut key = vec![Value::Null; 3];
        key[1] = Value::DateTime(gap);
        range.set_specific_row(key);
        let mut cursor =
            crate::cursor::IndexCursor::new_with_range(&mut db, "DT", "DIndex", &range).unwrap();
        cursor.before_first();
        let found = cursor.get_next_row(&mut db).unwrap().unwrap();
        assert_eq!(found[0], Value::Long(1));

        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    /// The property block order of a table created in this process: the
    /// column maps come first, in column order, and the default map follows
    /// (`TableImpl`'s constructor allocates the column maps first), which is
    /// what makes the written blob byte identical to Java's.
    #[test]
    fn created_table_property_map_order_matches_java() {
        let (path, mut db) = create_test_db("zoned_proporder");
        let spec = TableSpec::new(
            "DT",
            vec![
                ColumnSpec::new("A", DataType::Long),
                ColumnSpec::new("B", DataType::Long)
                    .put_property(
                        crate::property::DEFAULT_VALUE_PROP,
                        crate::value::Value::Text("7".to_string()),
                    )
                    .unwrap(),
            ],
        );
        db.create_table(&spec).unwrap();
        let mut maps = db.get_table_properties_by_name("DT").unwrap();
        maps.get_default()
            .put_value("Prop", crate::value::Value::Long(1))
            .unwrap();
        maps.get_mut("A")
            .put_value("Prop", crate::value::Value::Long(2))
            .unwrap();
        let names: Vec<String> = maps.iter().map(|m| m.name().to_string()).collect();
        assert_eq!(names, vec!["A".to_string(), "B".to_string(), String::new()]);
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}

/// Test scratch directory inside the workspace (the sandbox does not allow
/// writes outside it for spawned test processes).
#[cfg(all(test, feature = "fixtures"))]
fn test_tmp_dir() -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// Jet 3 (Access 97) fixture tests
//
// The fixtures are the real Access 97 files jackcess is tested against; the
// expected values are the ones `java-src/Jet3DumpGen.java` produces for the
// same files (`tools/verify-jet3.ps1` compares the full dumps).  A test whose
// fixture is not present is skipped, so the suite also runs in a checkout
// without `external/`.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod jet3_tests {
    use super::*;
    use crate::cursor::Cursor;
    use crate::database::Database;
    use crate::datenums::LocalDateTime;
    use crate::format::Charset;
    use crate::value::Value;

    fn fixture(name: &str) -> Option<std::path::PathBuf> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external")
            .join("jackcess-test-resources")
            .join("data")
            .join("V1997")
            .join(name);
        if path.exists() {
            Some(path)
        } else {
            eprintln!("skipping jet 3 fixture test: {} is missing", path.display());
            None
        }
    }

    fn open(name: &str) -> Option<Database> {
        Some(Database::open_path(&fixture(name)?).expect("open the jet 3 fixture"))
    }

    fn table_page(db: &Database, name: &str) -> (i32, i32) {
        *db.table_lookup
            .get(&name.to_uppercase())
            .unwrap_or_else(|| panic!("table {} is not in the catalog", name))
    }

    fn load(db: &mut Database, name: &str) -> i32 {
        let (page, flags) = table_page(db, name);
        db.load_table(page, name, flags).expect("load the table");
        page
    }

    /// The rows of `Table1` of `common1V1997.mdb`: two text columns which live
    /// in the Jet 3 "jump table" followed by the fixed length scalar types.
    #[test]
    fn jet3_jump_table_rows_match_the_java_reference() {
        let mut db = match open("common1V1997.mdb") {
            Some(db) => db,
            None => return,
        };
        let page = load(&mut db, "Table1");
        let table = db.tables.get(&page).unwrap();

        // the definition the Java dumper prints
        assert_eq!(table.row_count, 2);
        assert_eq!(table.max_column_count, 9);
        assert_eq!(table.max_var_column_count, 2);
        assert_eq!(table.columns.len(), 9);
        let names: Vec<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["A", "B", "C", "D", "E", "F", "G", "H", "I"]);
        // A and B are the variable length columns (jump table index 0 and 1),
        // the rest are fixed length
        assert!(table.columns[0].is_variable_length());
        assert!(table.columns[1].is_variable_length());
        assert_eq!(table.columns[0].var_len_table_index, 0);
        assert_eq!(table.columns[1].var_len_table_index, 1);
        assert!(table.columns[2..].iter().all(|c| !c.is_variable_length()));
        assert_eq!(table.columns[0].get_length(), 50);
        assert_eq!(table.columns[1].get_length(), 100);
        // the money column reports the type default precision/scale, like Java
        assert_eq!(table.columns[7].get_precision(), 19);
        assert_eq!(table.columns[7].get_scale(), 4);

        let rows = table.read_all_rows(&mut db.channel).expect("read rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].clone().expect("row 0"),
            vec![
                Value::Text("a".into()),
                Value::Text("b".into()),
                Value::Byte(0),
                Value::Int(0),
                Value::Long(0),
                Value::Double(0.0),
                Value::DateTime(LocalDateTime::of_date(1981, 12, 12)),
                Value::Decimal {
                    unscaled: 0,
                    scale: 4
                },
                Value::Bool(false),
            ]
        );
        assert_eq!(
            rows[1].clone().expect("row 1"),
            vec![
                Value::Text("abcdefg".into()),
                Value::Text("hijklmnop".into()),
                Value::Byte(2),
                Value::Int(222),
                Value::Long(333_333_333),
                Value::Double(f64::from_bits(0x407b_c8e1_47ae_147b)),
                Value::DateTime(LocalDateTime::of_date(1974, 9, 21)),
                Value::Decimal {
                    unscaled: 35000,
                    scale: 4
                },
                Value::Bool(true),
            ]
        );
    }

    /// The jump table packs each variable column boundary into one byte plus a
    /// 256 byte "jump" for each 256 bytes of row: both the short row form and
    /// the jumped form are checked here, with the offsets the Java
    /// `readJumpTableVarColOffsets` produces.
    #[test]
    fn jet3_jump_table_offsets_match_the_java_algorithm() {
        // a short row: no jumps, the offsets are the bytes just before the
        // null mask, read backwards from num_var_cols
        let mut buffer = ByteBuf::allocate(100);
        buffer.set_limit(100).unwrap();
        buffer.put_u8_at(98, 2).unwrap(); // num var cols, just before the mask
        buffer.put_u8_at(99, 0x03).unwrap(); // null mask
        buffer.put_u8_at(97, 5).unwrap();
        buffer.put_u8_at(96, 8).unwrap();
        buffer.put_u8_at(95, 10).unwrap();
        let mask = NullMask::read(&buffer, 99, 1).unwrap();
        assert_eq!(
            read_jump_table_var_col_offsets(&buffer, 0, &mask).unwrap(),
            vec![5, 8, 10]
        );

        // a row of 300 bytes: one 256 byte jump.  The byte after the null
        // mask is the jump table (a 0 marker), and the offsets get 256 added
        let mut buffer = ByteBuf::allocate(300);
        buffer.set_limit(300).unwrap();
        buffer.put_u8_at(298, 2).unwrap();
        buffer.put_u8_at(299, 0x03).unwrap();
        buffer.put_u8_at(297, 0).unwrap(); // the jump marker
        buffer.put_u8_at(296, 7).unwrap();
        buffer.put_u8_at(295, 20).unwrap();
        buffer.put_u8_at(294, 40).unwrap();
        let mask = NullMask::read(&buffer, 299, 1).unwrap();
        assert_eq!(
            read_jump_table_var_col_offsets(&buffer, 0, &mask).unwrap(),
            vec![7 + 256, 20 + 256, 40 + 256]
        );
    }

    /// `indexV1997.mdb` keeps deleted rows on the data page; the port reports
    /// them as `None` (with their row id), like the Java dump marks them
    /// `deleted`.
    #[test]
    fn jet3_deleted_rows_match_the_java_reference() {
        let mut db = match open("indexV1997.mdb") {
            Some(db) => db,
            None => return,
        };
        let page = load(&mut db, "MSysACEs");
        let table = db.tables.get(&page).unwrap();
        assert_eq!(table.row_count, 74);
        let rows = table
            .read_all_rows_with_ids(&mut db.channel)
            .expect("read rows");
        for row_num in 38..=42 {
            let id = (52, row_num);
            let (_, values) = rows
                .iter()
                .find(|(rid, _)| *rid == id)
                .unwrap_or_else(|| panic!("row {:?} is in the dump", id));
            assert!(values.is_none(), "row {:?} is deleted", id);
        }
        // the rows around them are intact
        let (_, before) = rows.iter().find(|(rid, _)| *rid == (52, 37)).unwrap();
        assert_eq!(
            before.clone().expect("row 52/37"),
            vec![
                Value::Long(-2147483641),
                Value::Bytes(vec![0x02, 0x01]),
                Value::Long(65536),
                Value::Bool(false),
            ]
        );
    }

    /// A row too large for its data page is moved to another page and the
    /// header row keeps an overflow pointer.  `indexCodesV1997.mdb` has four
    /// such rows in `MSysObjects`; the Java dump shows the same targets and
    /// values.
    #[test]
    fn jet3_overflow_rows_are_followed() {
        let mut db = match open("indexCodesV1997.mdb") {
            Some(db) => db,
            None => return,
        };
        let page = load(&mut db, "MSysObjects");
        let table = db.tables.get(&page).unwrap();
        assert_eq!(table.row_count, 51);

        let row_id = (18, 8);
        let loc = table
            .position_at_row_data(&mut db.channel, row_id)
            .expect("locate the overflow row");
        assert_eq!(loc.header_page_number, 18);
        assert_eq!(loc.header_row_number, 8);
        assert_eq!((loc.final_page_number, loc.final_row_number), (124, 17));
        let header_raw = loc
            .header_page
            .get_i16_at(get_row_start_offset(8, &db.format))
            .unwrap();
        assert!(is_overflow_row(header_raw));
        assert!(!is_deleted_row(header_raw));

        let values = table
            .read_row_at(&mut db.channel, row_id)
            .expect("read the overflow row");
        assert_eq!(values[0], Value::Long(-2147483648));
        assert_eq!(values[1], Value::Long(251_658_240));
        assert_eq!(values[2], Value::Text("DataAccessPages".into()));
        assert_eq!(values[3], Value::Int(3));
        assert_eq!(
            values[4],
            Value::DateTime(LocalDateTime::new(2019, 8, 20, 9, 19, 14, 504_000_000))
        );
    }

    /// A MEMO value longer than the inline maximum lives on other pages; the
    /// value in `common2V1997.mdb` is 348 characters.
    #[test]
    fn jet3_memo_long_value_matches_the_java_reference() {
        let mut db = match open("common2V1997.mdb") {
            Some(db) => db,
            None => return,
        };
        let page = load(&mut db, "MSP_PROJECTS");
        let table = db.tables.get(&page).unwrap();
        assert_eq!(table.columns.len(), 74);
        assert_eq!(table.row_count, 1);
        let rows = table.read_all_rows(&mut db.channel).expect("read rows");
        let row = rows[0].clone().expect("the single row");
        // the column order is the data order: PROJ_ID, PROJ_NAME, then
        // PROJ_PROP_AUTHOR
        let author = row[2].as_text().expect("PROJ_PROP_AUTHOR is text");
        assert_eq!(author.len(), 348);
        assert!(author.starts_with("Jon Iles this is a a vawesraso"));
        assert!(author.ends_with("fj lkasj dlkfslkd jflksjadf as"));
        // the long value lives on other pages (it is longer than the inline
        // maximum, 64 bytes), so following the long value definition worked
        assert!(author.len() > 64);
    }

    /// The index definitions and the index entries (in B-tree order) of
    /// `indexV1997.mdb`.
    #[test]
    fn jet3_indexes_match_the_java_reference() {
        let mut db = match open("indexV1997.mdb") {
            Some(db) => db,
            None => return,
        };
        let page = load(&mut db, "Table1");
        {
            let table = db.tables.get(&page).unwrap();
            assert_eq!(table.indexes.len(), 4);
            let pk = table
                .indexes
                .iter()
                .find(|i| i.name == "PrimaryKey")
                .expect("PrimaryKey index");
            assert!(pk.is_primary_key());
            assert!(!pk.is_foreign_key());
            let data = table.index_data_for(pk).expect("index data");
            assert!(data.is_unique());
            assert!(data.is_required());
            assert_eq!(data.unique_entry_count, 4);
            assert_eq!(data.columns.len(), 1);
            assert_eq!(data.columns[0].column.name, "id");
            assert!(data.columns[0].is_ascending());

            let fk = table
                .indexes
                .iter()
                .find(|i| i.name == "Table2Table1")
                .expect("the foreign key index");
            assert!(fk.is_foreign_key());
            assert!(fk.reference.is_some());
        }

        // the entries, read through the B-tree
        let mut cursor =
            crate::cursor::IndexCursor::new(&mut db, "Table1", "PrimaryKey").expect("index cursor");
        let mut ids = Vec::new();
        while cursor.get_next_row(&mut db).expect("next entry").is_some() {
            ids.push(cursor.get_current_row_id());
        }
        assert_eq!(ids, vec![(27, 0), (27, 1), (27, 2), (27, 3)]);
    }

    /// A Jet 3 database always opens read only (jackcess cannot write it), and
    /// a write fails with the `NonWritableChannelException` a read only
    /// `FileChannel` throws in Java.
    #[test]
    fn jet3_database_opens_read_only_and_refuses_writes() {
        let mut db = match open("common1V1997.mdb") {
            Some(db) => db,
            None => return,
        };
        assert!(db.is_read_only());
        assert_eq!(db.charset, Charset::Cp1252);
        assert_eq!(db.default_code_page, Some(1252));
        assert_eq!(db.format.name, "VERSION_3");

        let buffer = db.channel.create_page_buffer();
        db.channel.start_write();
        let err = db
            .channel
            .write_page(&buffer, 3)
            .expect_err("write is refused");
        let _ = db.channel.finish_write();
        assert!(
            err.to_string().contains("NonWritableChannelException"),
            "unexpected error: {}",
            err
        );
    }

    /// The system catalog of a Jet 3 database is read like any other table.
    #[test]
    fn jet3_system_catalog_is_read() {
        let mut db = match open("common1V1997.mdb") {
            Some(db) => db,
            None => return,
        };
        let user = db.get_table_names().expect("user table names");
        assert_eq!(user, vec!["Table1", "Table2", "Table3", "Table4"]);
        let system = db.get_system_table_names().expect("system table names");
        assert_eq!(
            system,
            vec![
                "MSysACEs",
                "MSysModules",
                "MSysModules2",
                "MSysObjects",
                "MSysQueries",
                "MSysRelationships"
            ]
        );
        let page = load(&mut db, "MSysObjects");
        let table = db.tables.get(&page).unwrap();
        assert_eq!(table.row_count, 22);
        assert_eq!(table.columns.len(), 17);
        let rows = table.read_all_rows(&mut db.channel).expect("read catalog");
        let live = rows.iter().filter(|r| r.is_some()).count();
        assert_eq!(live, 22);
    }
}

#[cfg(test)]
mod jet3_text_fixture_tests {

    use crate::database::Database;
    use crate::value::Value;

    /// A text value which holds one of the windows-1252 bytes the charset
    /// leaves undefined: Java decodes it to U+FFFD, and so does the port (the
    /// charset comes from the code page in the file header, not from the
    /// format default).
    #[test]
    fn jet3_text_decoding_uses_the_header_code_page() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external")
            .join("jackcess-test-resources")
            .join("data")
            .join("V1997")
            .join("indexCodesV1997.mdb");
        if !path.exists() {
            eprintln!("skipping jet 3 fixture test: {} is missing", path.display());
            return;
        }
        let mut db = Database::open_path(&path).expect("open the fixture");
        assert_eq!(db.charset, crate::format::Charset::Cp1252);
        let (page, flags) = *db.table_lookup.get("TABLE1").expect("Table1");
        db.load_table(page, "Table1", flags).expect("load Table1");
        let table = db.tables.get(&page).unwrap();
        // the values of row 47/34 in the Java dump
        let values = table
            .read_row_at(&mut db.channel, (47, 34))
            .expect("read row 47/34");
        assert_eq!(values[0], Value::Text("row129".into()));
        assert_eq!(values[1], Value::Text("AA\u{fffd}AA".into()));
    }
}

#[cfg(test)]
mod external_validator_tests {
    use super::*;
    use crate::database::Database;
    use crate::format::FileFormat;
    use crate::tablecreator::TableSpec;

    struct Reject;
    impl crate::validator::ColumnValidator for Reject {
        fn validate(
            &self,
            _context: &crate::validator::ColumnValidatorContext<'_>,
            _value: Value,
        ) -> Result<Value> {
            Err(Error::InvalidValue("custom validator rejected".to_string()))
        }
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from("target/test-tmp");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!(
            "jackcess_ext_validator_{}_{}.mdb",
            name,
            std::process::id()
        ))
    }

    #[test]
    fn external_column_validator_runs_on_insert() {
        let path = tmp("insert");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        );
        db.create_table(&spec).unwrap();
        let page = db.table_lookup.get("T").unwrap().0;
        {
            let table = db.tables.get_mut(&page).unwrap();
            table.columns[1]
                .set_column_validator(Some(crate::validator::ColumnValidatorHandle::new(Reject)));
        }
        let err = db
            .add_rows("T", &[vec![Value::Null, Value::Text("x".into())]])
            .unwrap_err();
        assert!(
            err.to_string().contains("custom validator rejected"),
            "unexpected error: {err}"
        );
        drop(db);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn auto_number_marker_generates_on_add_and_keeps_on_update() {
        let path = tmp("auto_marker");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        );
        db.create_table(&spec).unwrap();
        let marker = Value::Text(crate::column::AUTO_NUMBER.to_string());
        db.add_rows("T", &[vec![marker.clone(), Value::Text("a".into())]])
            .unwrap();
        let first = db.read_rows_with_ids("T").unwrap()[0].1.clone().unwrap();
        let id = first[0].as_i64().expect("generated auto number");
        assert!(id > 0);
        // an update with the marker keeps the existing value
        let row_id = db.read_rows_with_ids("T").unwrap()[0].0;
        db.update_row("T", row_id, &[marker, Value::Text("b".into())])
            .unwrap();
        let after = db.read_rows_with_ids("T").unwrap()[0].1.clone().unwrap();
        assert_eq!(after[0].as_i64(), Some(id));
        assert_eq!(after[1].as_text(), Some("b"));
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
}
