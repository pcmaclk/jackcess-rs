//! Port of `ColumnImpl` and its subclasses (definitions and value codecs).
//!
//! # Java name mapping (the `Column` object API)
//!
//! | Java (`com.healthmarketscience.jackcess.Column`) | Rust |
//! |---|---|
//! | `getTable()` | [`Column::get_table`] (the owning table is passed in; a Rust column keeps no back reference) |
//! | `getDatabase()` | [`Column::get_database`] (the caller supplies the owning database) |
//! | `getName()` | field [`Column::name`] |
//! | `isVariableLength()` | [`Column::is_variable_length`] |
//! | `isAutoNumber()` | field [`Column::auto_number`] |
//! | `getColumnIndex()` | [`Column::get_column_index`] |
//! | `getColumnId()` | [`Column::get_column_id`] |
//! | `getType()` | [`Column::get_type`] |
//! | `getSQLType()` | [`Column::get_sql_type`] |
//! | `isCompressedUnicode()` | field [`Column::compressed_unicode`] |
//! | `getPrecision()` / `getScale()` | [`Column::get_precision`] / [`Column::get_scale`] |
//! | `getLength()` / `getLengthInUnits()` | [`Column::get_length`] / [`Column::get_length_in_units`] |
//! | `isAppendOnly()` | field [`Column::append_only`] (also `Database::is_append_only`)* |
//! | `isHyperlink()` | [`Column::is_hyperlink`] |
//! | `isCalculated()` / `isHidden()` | fields [`Column::calculated`] / [`Column::hidden`] |
//! | `isSecurityIdentifier()` | [`Column::is_security_identifier`] |
//! | `getComplexInfo()` | [`Column::get_complex_info`] |
//! | `getProperties()` | [`Column::get_properties`] |
//! | `getVersionHistoryColumn()` | [`Column::get_version_history_column`] |
//! | `getColumnValidator()` / `setColumnValidator(...)` | [`Column::get_column_validator`] / [`Column::set_column_validator`] |
//! | `setRowValue(Object[], Object)` / `setRowValue(Map, Object)` | [`Column::set_row_value`] / [`Column::set_row_value_map`] |
//! | `getRowValue(Object[])` / `getRowValue(Map)` | [`Column::get_row_value`] / [`Column::get_row_value_map`] |
//!
//! \* the accessors marked with an asterisk are the pre-existing field based
//! APIs of this port (kept unchanged); every other row is the Java object API
//! added on top of them.

use crate::bytebuf::{ByteBuf, ByteOrder};
use crate::database::DateTimeType;
use crate::datatype::DataType;
use crate::datenums::{self, LocalDateTime, TimeZone};
use crate::error::{Error, Result};
use crate::format::{Charset, JetFormat, SortOrder};
use crate::pagechannel::PageChannel;
use crate::property::Property;
use crate::value::Value;

/// flag bits
pub const FIXED_LEN_FLAG_MASK: u8 = 0x01;
pub const UPDATABLE_FLAG_MASK: u8 = 0x02;
pub const AUTO_NUMBER_FLAG_MASK: u8 = 0x04;
pub const HIDDEN_FLAG_MASK: u8 = 0x10;
pub const SECURITY_IDENTIFIER_FLAG_MASK: u8 = 0x20;
pub const AUTO_NUMBER_GUID_FLAG_MASK: u8 = 0x40;
pub const HYPERLINK_FLAG_MASK: u8 = 0x80;

pub const COMPRESSED_UNICODE_EXT_FLAG_MASK: u8 = 0x01;
pub const COMPLEX_FK_EXT_FLAG_MASK: u8 = 0x08;
pub const VERSION_HISTORY_EXT_FLAG_MASK: u8 = 0x20;
pub const CALCULATED_EXT_FLAG_MASK: u8 = 0xC0;

// ---------------------------------------------------------------------------
// calculated column value wrapper (`CalculatedColumnUtil`)
// ---------------------------------------------------------------------------

/// offset to the int which specifies the length of the actual data
const CALC_DATA_LEN_OFFSET: usize = 16;
/// offset to the actual data
const CALC_DATA_OFFSET: usize = 20;
/// total amount of extra bytes added for calculated values
pub const CALC_EXTRA_DATA_LEN: usize = 23;
/// ms access seems to define all fixed-len calc fields as this length
pub const CALC_FIXED_FIELD_LEN: i16 = 39;

/// `Column.AUTO_NUMBER`: a placeholder value which asks an auto-number column
/// to generate a fresh value (it behaves like a missing value; on an update it
/// keeps the existing value, like `KEEP_VALUE`).  It is a plain text marker so
/// it can be passed through the ordinary `Vec<Value>` row APIs.
pub const AUTO_NUMBER: &str = "<AUTO_NUMBER>";

/// Whether the value is the [`AUTO_NUMBER`] placeholder.
pub fn is_auto_number_value(value: &Value) -> bool {
    matches!(value, Value::Text(text) if text == AUTO_NUMBER)
}

/// `CalculatedColumnUtil.wrapCalculatedValue(byte[])`: prefixes the data with
/// the calculated value header (the data length at offset 16) and pads the end
/// to [`CALC_EXTRA_DATA_LEN`] extra bytes.
pub fn wrap_calculated_value(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; data.len() + CALC_EXTRA_DATA_LEN];
    out[CALC_DATA_LEN_OFFSET..CALC_DATA_LEN_OFFSET + 4]
        .copy_from_slice(&(data.len() as u32).to_le_bytes());
    out[CALC_DATA_OFFSET..CALC_DATA_OFFSET + data.len()].copy_from_slice(data);
    out
}

/// `CalculatedColumnUtil.unwrapCalculatedValue(byte[])`: grabs the real data
/// bytes from a calculated value.  The length field is always read in the
/// default (little endian) byte order, like `PageChannel.wrap`.
pub fn unwrap_calculated_value(data: &[u8]) -> Vec<u8> {
    if data.len() < CALC_DATA_OFFSET {
        return data.to_vec();
    }
    let mut len_bytes = [0u8; 4];
    len_bytes.copy_from_slice(&data[CALC_DATA_LEN_OFFSET..CALC_DATA_LEN_OFFSET + 4]);
    let data_len = i32::from_le_bytes(len_bytes);
    let remaining = (data.len() - CALC_DATA_OFFSET) as i32;
    let take = remaining.min(data_len).max(0) as usize;
    data[CALC_DATA_OFFSET..CALC_DATA_OFFSET + take].to_vec()
}

pub const TEXT_COMPRESSION_HEADER: [u8; 2] = [0xFF, 0xFE];
const MIN_COMPRESS_CHAR: u32 = 1;
const MAX_COMPRESS_CHAR: u32 = 0xFF;

/// A read column definition (`ColumnImpl`).
#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub data_type: DataType,
    pub column_number: i16,
    pub column_id: i16,
    pub column_length: i16,
    pub variable_length: bool,
    pub auto_number: bool,
    pub calculated: bool,
    pub complex_value_fk: bool,
    pub hidden: bool,
    pub security_identifier: bool,
    pub var_len_table_index: i16,
    pub fixed_data_offset: i16,
    pub flags: u8,
    pub ext_flags: u8,
    pub precision: u8,
    pub scale: u8,
    pub sort_order: Option<SortOrder>,
    pub code_page: i16,
    pub compressed_unicode: bool,
    pub display_index: usize,
    /// index of this column in the sorted table column list (Java columnIndex)
    pub column_index: usize,
    /// long value usage map info (read from the tdef trailer)
    pub umap_page_number: i32,
    pub umap_owned_row_number: u8,
    pub umap_free_row_number: u8,
    /// append-only (version history) memo column
    pub append_only: bool,
    /// the `MSysComplexColumns` row id of a `COMPLEX_TYPE` column (the four
    /// bytes which hold the sort order of a text column)
    pub complex_id: i32,
    /// cached length in units
    pub length_in_units: i32,
    /// the trimmed `DefaultValue` property, when the table's property maps
    /// have been loaded (`ColumnImpl._defValue`'s expression string)
    pub default_value: Option<String>,
    /// the `Required` property (default `false`), enforced on row add/update
    /// by the port of `ColumnImpl.RequiredColValidator`
    pub required: bool,
    /// the `AllowZeroLength` property (default `true`, textual columns only),
    /// enforced on row add/update by the port of
    /// `ColumnImpl.NoZeroLenColValidator`
    pub allow_zero_length: bool,
    /// the user ("external") column validator (`ColumnImpl._validator`'s custom
    /// slot).  Java's internal validator chain wraps it with the property
    /// driven checks; the port stores the custom part here and applies the
    /// property driven checks in the table's row path.  `None` is Java's
    /// `SimpleColumnValidator`.
    pub column_validator: Option<crate::validator::ColumnValidatorHandle>,
}

/// The internal (property driven) column validator failures, mirroring the
/// exceptions `RequiredColValidator` / `NoZeroLenColValidator` throw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnValidation {
    /// `Required` is on and the value is null
    Required,
    /// `AllowZeroLength` is off and the value is an empty string
    ZeroLength,
    /// `AllowZeroLength` is off and the value cannot be converted to text
    NotText,
}

/// An expression based validation rule, the port of the
/// `ColumnImpl.reloadPropertiesValidators` column rule
/// (`ColValidatorEvalContext`, `Expressionator.Type.FIELD_VALIDATOR`) and of
/// the table level row rule (`TableImpl.reloadRowValidator`,
/// `RowValidatorEvalContext`, `Expressionator.Type.RECORD_VALIDATOR`).
///
/// The rule is the trimmed `ValidationRule` property; `ValidationText` (also
/// trimmed) replaces the default rejection message exactly like the Java
/// library.  Java parses the rule lazily on the first evaluation
/// (`BaseEvalContext.RawExpr`), so a parse failure is kept here and reported
/// when the rule runs instead of failing the property load.
#[derive(Clone, Debug)]
pub struct ValidationRule {
    /// the parsed expression, or the parse error Java would raise on the first
    /// evaluation
    pub expression: std::result::Result<crate::expr::Expression, crate::expr::ExprError>,
    /// the trimmed `ValidationText` property, when set
    pub text: Option<String>,
}

impl ValidationRule {
    /// A field rule (`Expressionator.Type.FIELD_VALIDATOR`): `[ThisColumn]` is
    /// the implicit left value and the column itself is the only field the
    /// expression may read.
    pub fn field(expr: &str, text: Option<&str>) -> ValidationRule {
        ValidationRule::parse(expr, text, crate::expr::ParseOptions::field_validator())
    }

    /// A record rule (`Expressionator.Type.RECORD_VALIDATOR`): the identifiers
    /// resolve against the row's columns.
    pub fn row(expr: &str, text: Option<&str>) -> ValidationRule {
        ValidationRule::parse(expr, text, crate::expr::ParseOptions::row_validator())
    }

    fn parse(expr: &str, text: Option<&str>, options: crate::expr::ParseOptions) -> ValidationRule {
        ValidationRule {
            expression: crate::expr::parse_with(expr, options),
            text: text.map(|t| t.to_string()),
        }
    }

    /// The rejection message: the `ValidationText` when it is set, otherwise
    /// the default message built by `default` (Java's
    /// `((_helpStr != null) ? _helpStr : <default>)`).
    pub fn message<F: FnOnce() -> String>(&self, default: F) -> String {
        match &self.text {
            Some(text) => text.clone(),
            None => default(),
        }
    }
}

/// The text Java's string concatenation produces for a row value, used by the
/// default column rejection message
/// (`"Invalid column value '" + val + "'"`).
///
/// Only the deterministic value kinds are covered; the identity based ones
/// (`byte[]`, `java.util.UUID` is handled) are formatted as their Java class
/// name would be, which is only observable for a message that a Java caller
/// could not reproduce deterministically either.
pub fn value_message_text(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        // java.lang.Byte.toString is signed
        Value::Byte(b) => (*b as i8).to_string(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::BigInt(v) => v.to_string(),
        Value::Decimal { unscaled, scale } => {
            crate::expr::Decimal::from_unscaled(*unscaled, *scale)
                .map(|d| d.to_plain_string())
                .unwrap_or_else(|_| format!("{unscaled}E-{scale}"))
        }
        Value::Float(v) => java_double_text(*v as f64),
        Value::Double(v) => java_double_text(*v),
        Value::Text(s) => s.clone(),
        Value::Bytes(_) => "[B".to_string(),
        Value::DateTime(v) => java_local_date_time_text(v),
        Value::DateBits(bits) => java_local_date_time_text(&ldt_from_local_date_double(*bits)),
        Value::Uuid(bytes) => format_uuid(bytes),
        Value::Keep => "<KEEP_VALUE>".to_string(),
    }
}

/// `Double.toString` for the values a message can hold (Java prints integral
/// doubles with a trailing `.0` and a negative zero as `-0.0`).
fn java_double_text(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if v == 0.0 {
        return if v.is_sign_negative() { "-0.0" } else { "0.0" }.to_string();
    }
    let text = format!("{v}");
    if text.contains('.') || text.contains('e') || text.contains('E') {
        text
    } else {
        format!("{text}.0")
    }
}

/// `LocalDateTime.toString`: the seconds are omitted when they are zero, and
/// the fraction is printed as 3, 6 or 9 digits (Java prints the shortest of
/// those groups which holds the nanoseconds, so `.123` stays `.123` while
/// `.123456700` keeps all nine digits).
pub fn java_local_date_time_text(value: &crate::datenums::LocalDateTime) -> String {
    // `LocalDate.toString`: a year outside 0000..9999 is prefixed with its
    // sign, and the year field keeps at least four digits
    let year = if value.year > 9999 {
        format!("+{}", value.year)
    } else if value.year < 0 {
        format!("-{:04}", -value.year)
    } else {
        format!("{:04}", value.year)
    };
    let mut out = format!(
        "{}-{:02}-{:02}T{:02}:{:02}",
        year, value.month, value.day, value.hour, value.minute
    );
    if value.second != 0 || value.nano != 0 {
        out.push_str(&format!(":{:02}", value.second));
        if value.nano != 0 {
            let digits = if value.nano.is_multiple_of(1_000_000) {
                3
            } else if value.nano.is_multiple_of(1_000) {
                6
            } else {
                9
            };
            let frac = format!("{:09}", value.nano);
            out.push('.');
            out.push_str(&frac[..digits]);
        }
    }
    out
}

/// `java.util.UUID.toString`.
fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

impl Column {
    pub fn get_type(&self) -> DataType {
        self.data_type
    }

    pub fn is_variable_length(&self) -> bool {
        self.variable_length
    }

    pub fn get_length(&self) -> i16 {
        self.column_length
    }

    pub fn fixed_data_size(&self) -> i32 {
        self.data_type
            .get_fixed_size_with_length(Some(self.column_length))
    }

    /// `ColumnImpl.getPrecision`: the type default unless the column keeps the
    /// value in its definition (`NumericColumnImpl`, i.e. a data type which has
    /// scale/precision).
    pub fn get_precision(&self) -> u8 {
        if self.data_type.has_scale_precision() {
            self.precision
        } else {
            self.data_type.get_default_precision() as u8
        }
    }

    /// `ColumnImpl.getScale`, the counterpart of [`Column::get_precision`].
    pub fn get_scale(&self) -> u8 {
        if self.data_type.has_scale_precision() {
            self.scale
        } else {
            self.data_type.get_default_scale() as u8
        }
    }

    pub fn store_in_null_mask(&self) -> bool {
        // calculated booleans are _not_ stored in the null mask
        // (`CalcBooleanColImpl.storeInNullMask`)
        self.data_type == DataType::Boolean && !self.calculated
    }

    pub fn write_to_null_mask(&self, value: &Value) -> bool {
        to_boolean_value(value)
    }

    pub fn read_from_null_mask(&self, is_null: bool) -> Value {
        Value::Bool(!is_null)
    }

    pub fn length_in_units(&self, format: &JetFormat) -> i32 {
        if self.length_in_units >= 0 {
            self.length_in_units
        } else {
            self.data_type
                .to_unit_size(self.column_length as i32, Some(format))
        }
    }

    /// `CalcTextColImpl.calcLengthInUnits`: the byte "length" of a calculated
    /// text column includes the calculated field overhead, so the actual data
    /// length in units is smaller by that much.
    pub fn calculated_text_units(&self, format: &JetFormat) -> i32 {
        self.data_type.to_unit_size(
            self.column_length as i32 - CALC_EXTRA_DATA_LEN as i32,
            Some(format),
        )
    }

    /// The maximum text length in units of a long value column data
    /// (`LongValueColumnImpl.calcMaxLengthInUnits`, with the
    /// `CalcMemoColImpl.calcMaxLengthInUnits` adjustment for a calculated
    /// memo column).
    pub fn long_value_max_units(&self, format: &JetFormat) -> i32 {
        let max_size = self.data_type.get_max_size();
        let max_size = if self.calculated {
            max_size - CALC_EXTRA_DATA_LEN as i32
        } else {
            max_size
        };
        self.data_type.to_unit_size(max_size, Some(format))
    }

    /// `ColumnImpl.generateDefaultValue`: evaluates this column's
    /// `DefaultValue` property.  The Java library parses the property as an
    /// Access expression; the port supports the literal forms which can be
    /// reproduced exactly (see [`eval_default_value`]).  A property starting
    /// with `=` is full expression mode, which the port reports as
    /// [`Error::Unsupported`] instead of silently writing a different value.
    pub fn generate_default_value(&self) -> Result<Value> {
        match &self.default_value {
            None => Ok(Value::Null),
            Some(expr) => eval_default_value(self, expr),
        }
    }

    /// `ColumnImpl.validate` for the internal (property driven) validators,
    /// i.e. `RequiredColValidator` and `NoZeroLenColValidator`.
    ///
    /// The validators wrap each other in the order the Java library creates
    /// them: `Required` first, then (for textual columns with the property
    /// off) `AllowZeroLength` on the outside, so a null value is reported as
    /// missing while an empty string is reported as zero length.  The zero
    /// length validator hands back the value's `CharSequence` form, which the
    /// port mirrors by returning the text it converted.
    pub fn validate_value(&self, value: Value) -> std::result::Result<Value, ColumnValidation> {
        if self.required && value.is_null() {
            return Err(ColumnValidation::Required);
        }
        if !self.allow_zero_length && self.data_type.is_textual() && !value.is_null() {
            // `ColumnImpl.toCharSequence`: null is allowed by the zero length
            // validator, every other value is converted to text first
            let text = match to_char_sequence(&value) {
                Ok(text) => text,
                // `ColumnImpl.toCharSequence` has a `toString` fallback for
                // unknown types; the port refuses the ones it cannot format
                // deterministically instead of writing a different string
                Err(_) => return Err(ColumnValidation::NotText),
            };
            if text.is_empty() {
                return Err(ColumnValidation::ZeroLength);
            }
            return Ok(Value::Text(text));
        }
        Ok(value)
    }

    /// Reads a column definition from a table definition buffer.
    pub fn read(
        buffer: &ByteBuf,
        offset: usize,
        name: String,
        display_index: usize,
        format: &JetFormat,
    ) -> Result<Column> {
        let column_number = buffer.get_i16_at(offset + format.offset_column_number)?;
        let column_id = buffer.get_i16_at(offset + format.offset_column_id)?;
        let column_length = buffer.get_i16_at(offset + format.offset_column_length)?;
        let flags = buffer.get_u8_at(offset + format.offset_column_flags)?;
        let ext_flags = if format.offset_column_ext_flags >= 0 {
            buffer.get_u8_at(offset + format.offset_column_ext_flags as usize)?
        } else {
            0
        };
        let variable_length = (flags & FIXED_LEN_FLAG_MASK) == 0;
        let auto_number = (flags & (AUTO_NUMBER_FLAG_MASK | AUTO_NUMBER_GUID_FLAG_MASK)) != 0;
        let calculated = (ext_flags & CALCULATED_EXT_FLAG_MASK) != 0;
        let complex_value_fk = (ext_flags & COMPLEX_FK_EXT_FLAG_MASK) != 0;
        let hidden = (flags & HIDDEN_FLAG_MASK) != 0;
        let security_identifier = (flags & SECURITY_IDENTIFIER_FLAG_MASK) != 0;
        let var_len_table_index =
            buffer.get_i16_at(offset + format.offset_column_variable_table_index)?;
        let fixed_data_offset =
            buffer.get_i16_at(offset + format.offset_column_fixed_data_offset)?;

        let col = Column {
            name,
            data_type: DataType::UnsupportedFixedLen,
            column_number,
            column_id,
            column_length,
            variable_length,
            auto_number,
            calculated,
            complex_value_fk,
            hidden,
            security_identifier,
            var_len_table_index,
            fixed_data_offset,
            flags,
            ext_flags,
            precision: 0,
            scale: 0,
            sort_order: None,
            code_page: 0,
            compressed_unicode: false,
            display_index,
            column_index: display_index,
            umap_page_number: -1,
            umap_owned_row_number: 0,
            umap_free_row_number: 0,
            append_only: false,
            complex_id: 0,
            length_in_units: -1,
            default_value: None,
            required: false,
            allow_zero_length: true,
            column_validator: None,
        };
        Ok(col)
    }

    /// Completes the type-specific parts of a definition read, once the data
    /// type is known (`ColumnImpl.create`).
    pub fn set_type(
        &mut self,
        data_type: DataType,
        buffer: &ByteBuf,
        offset: usize,
        format: &JetFormat,
    ) -> Result<()> {
        self.data_type = data_type;
        match data_type {
            DataType::Text | DataType::Memo => {
                self.sort_order = Some(read_sort_order(
                    buffer,
                    offset + format.offset_column_sort_order,
                    format,
                )?);
                self.code_page = read_code_page(buffer, offset, format)?;
                self.compressed_unicode = (self.ext_flags & COMPRESSED_UNICODE_EXT_FLAG_MASK) != 0;
                self.append_only = (self.ext_flags & VERSION_HISTORY_EXT_FLAG_MASK) != 0;
            }
            DataType::Numeric => {
                self.precision = if format.offset_column_precision > 0 {
                    buffer.get_u8_at(offset + format.offset_column_precision)?
                } else {
                    0
                };
                self.scale = if format.offset_column_scale > 0 {
                    buffer.get_u8_at(offset + format.offset_column_scale)?
                } else {
                    0
                };
            }
            DataType::ComplexType => {
                // the four bytes which hold the sort order of a text column
                // hold the id of the MSysComplexColumns row
                self.complex_id = if format.offset_column_complex_id > 0 {
                    buffer.get_i32_at(offset + format.offset_column_complex_id as usize)?
                } else {
                    0
                };
            }
            _ => {}
        }
        if data_type == DataType::Text {
            self.compressed_unicode = (self.ext_flags & COMPRESSED_UNICODE_EXT_FLAG_MASK) != 0;
        }
        Ok(())
    }

    /// Deserialize a raw byte value (`ColumnImpl.read`).
    ///
    /// `date_time_type` is the owning database's `DateTimeType`
    /// (`ColumnImpl.getDateTimeFactory`, which asks the database): a
    /// `SHORT_DATE_TIME` column decodes to the representation the configured
    /// type selects (see [`date_value_from_bits`]).
    pub fn read_value(
        &self,
        data: &[u8],
        order: ByteOrder,
        charset: Charset,
        format: &JetFormat,
        date_time_type: DateTimeType,
    ) -> Result<Value> {
        if self.calculated {
            return self.read_calculated_value(data, order, charset, format, date_time_type);
        }
        self.read_plain_value(data, order, charset, format, date_time_type)
    }

    fn read_plain_value(
        &self,
        data: &[u8],
        order: ByteOrder,
        charset: Charset,
        format: &JetFormat,
        date_time_type: DateTimeType,
    ) -> Result<Value> {
        let mut buf = ByteBuf::wrap(data.to_vec(), order);
        match self.data_type {
            DataType::Boolean => Err(Error::Jackcess(
                "Tried to read a boolean from data instead of null mask.".into(),
            )),
            DataType::Byte => Ok(Value::Byte(buf.get_u8()?)),
            DataType::Int => Ok(Value::Int(buf.get_i16()?)),
            DataType::Long => Ok(Value::Long(buf.get_i32()?)),
            DataType::Double => Ok(Value::Double(buf.get_f64()?)),
            DataType::Float => Ok(Value::Float(buf.get_f32()?)),
            DataType::ShortDateTime => {
                let bits = buf.get_u64()?;
                Ok(date_value_from_bits(bits, date_time_type))
            }
            DataType::Binary | DataType::BigBinary => Ok(Value::Bytes(data.to_vec())),
            DataType::Text => Ok(Value::Text(decode_text_value(
                data,
                charset,
                format,
                self.compressed_unicode,
            ))),
            DataType::Money => {
                let raw = buf.get_i64()?;
                Ok(Value::Decimal {
                    unscaled: raw as i128,
                    scale: 4,
                })
            }
            DataType::Numeric => {
                let negate = buf.get_u8()? != 0;
                let mut tmp = buf.get_bytes(16)?;
                if order != ByteOrder::BigEndian {
                    fix_numeric_byte_order(&mut tmp);
                }
                Ok(Value::Decimal {
                    unscaled: bytes_to_unscaled(&tmp, negate),
                    scale: self.scale as i32,
                })
            }
            DataType::Guid => Ok(Value::Uuid(read_guid_bytes(&mut buf)?)),
            DataType::ExtDateTime => Ok(Value::Text(String::new())),
            DataType::BigInt => Ok(Value::BigInt(buf.get_i64()?)),
            DataType::ComplexType => {
                // `ComplexValueForeignKeyImpl` is a Number; the port keeps the
                // foreign key id itself (the `complex` module wraps it)
                Ok(Value::Long(buf.get_i32()?))
            }
            _ => Err(Error::Jackcess(format!(
                "Unrecognized data type: {}",
                self.data_type.get_name()
            ))),
        }
    }

    /// Serialize a value the way `writeRealData` does (no raw-data wrapper).
    ///
    /// `date_time_type` is the owning database's `DateTimeType`: it selects the
    /// `SHORT_DATE_TIME` write conversion (`ColumnImpl.writeDateValue` asks the
    /// column's `DateTimeFactory`).
    pub fn write_value(
        &self,
        value: &Value,
        order: ByteOrder,
        charset: Charset,
        format: &JetFormat,
        tz: &TimeZone,
        date_time_type: DateTimeType,
    ) -> Result<Vec<u8>> {
        if self.calculated {
            return self.write_calculated_value(value, order, charset, format, tz, date_time_type);
        }
        self.write_plain_value(value, order, charset, format, tz, date_time_type)
    }

    fn write_plain_value(
        &self,
        value: &Value,
        order: ByteOrder,
        charset: Charset,
        format: &JetFormat,
        tz: &TimeZone,
        date_time_type: DateTimeType,
    ) -> Result<Vec<u8>> {
        if !self.variable_length || !self.data_type.is_variable_length() {
            return self.write_fixed_length_field(
                value,
                order,
                charset,
                format,
                tz,
                date_time_type,
            );
        }
        match self.data_type {
            DataType::Numeric => {
                let mut buf =
                    ByteBuf::allocate_order(self.data_type.get_fixed_size() as usize, order);
                self.write_numeric_value(&mut buf, value)?;
                let n = buf.position();
                Ok(buf.backing()[..n].to_vec())
            }
            DataType::Text => {
                let max = self.length_in_units(format);
                let mut bytes = encode_text_value(
                    value,
                    0,
                    max,
                    false,
                    self.compressed_unicode,
                    charset,
                    format,
                )?;
                bytes.shrink_to_fit();
                Ok(bytes)
            }
            DataType::Binary | DataType::UnsupportedVarLen => Ok(to_byte_array(value)?),
            _ => Err(Error::Jackcess(format!(
                "unexpected inline var length type: {}",
                self.data_type.get_name()
            ))),
        }
    }

    pub fn write_fixed_length_field(
        &self,
        value: &Value,
        order: ByteOrder,
        charset: Charset,
        format: &JetFormat,
        tz: &TimeZone,
        date_time_type: DateTimeType,
    ) -> Result<Vec<u8>> {
        let size = self.fixed_data_size();
        let mut buffer = ByteBuf::allocate_order(size as usize, order);
        let value = boolean_to_integer(value);
        match self.data_type {
            DataType::Boolean => {}
            DataType::Byte => buffer.put_u8(to_number(&value)? as i8 as u8)?,
            DataType::Int => buffer.put_i16(to_number(&value)? as i16)?,
            DataType::Long => buffer.put_i32(to_number(&value)? as i32)?,
            DataType::Money => {
                let d = to_decimal(&value)?;
                let scaled = (d * 10000.0).round() as i64;
                buffer.put_i64(scaled)?;
            }
            DataType::Float => buffer.put_f32(to_number(&value)? as f32)?,
            DataType::Double => buffer.put_f64(to_number(&value)?)?,
            DataType::ShortDateTime => {
                let bits = date_value_bits_with(&value, tz, date_time_type)?;
                buffer.put_u64(bits)?;
            }
            DataType::Text => {
                let num_chars = self.length_in_units(format);
                let bytes =
                    encode_text_value(&value, num_chars, num_chars, true, true, charset, format)?;
                buffer.put_bytes(&bytes)?;
            }
            DataType::Guid => {
                write_guid_value(&mut buffer, &value)?;
            }
            DataType::Numeric => {
                self.write_numeric_value(&mut buffer, &value)?;
            }
            DataType::Binary | DataType::BigBinary | DataType::ComplexType => {
                buffer.put_i32(to_number(&value)? as i32)?;
            }
            DataType::BigInt => buffer.put_i64(to_number(&value)? as i64)?,
            _ => {
                return Err(Error::Jackcess(format!(
                    "Unsupported data type: {}",
                    self.data_type.get_name()
                )))
            }
        }
        let n = buffer.position();
        Ok(buffer.backing()[..n].to_vec())
    }

    fn write_numeric_value(&self, buffer: &mut ByteBuf, value: &Value) -> Result<()> {
        let dec = to_decimal(value)?;
        let negate = dec < 0.0;
        let abs = dec.abs();
        buffer.put_u8(if negate { 0x80 } else { 0 })?;
        let scale = self.scale as i32;
        let unscaled = (abs * 10f64.powi(scale)).round() as i128;
        let mut bytes = unscaled_to_bytes(unscaled, self.data_type.get_fixed_size() as usize - 1);
        if buffer.order() != ByteOrder::BigEndian {
            fix_numeric_byte_order(&mut bytes);
        }
        buffer.put_bytes(&bytes)?;
        Ok(())
    }

    // -----------------------------------------------------------------
    // calculated column values (`CalculatedColumnUtil`)
    // -----------------------------------------------------------------

    /// The `read` implementations of `CalculatedColumnUtil`'s column classes:
    /// the stored bytes are a calculated value wrapper around the real data.
    fn read_calculated_value(
        &self,
        data: &[u8],
        order: ByteOrder,
        charset: Charset,
        format: &JetFormat,
        date_time_type: DateTimeType,
    ) -> Result<Value> {
        let data = unwrap_calculated_value(data);
        match self.data_type {
            // `CalcBooleanColImpl.read`: an empty value is FALSE
            DataType::Boolean => Ok(Value::Bool(data.first().map(|b| *b != 0).unwrap_or(false))),
            // `CalcTextColImpl.read` / `CalcMemoColImpl.readLongValue`
            DataType::Text | DataType::Memo => Ok(Value::Text(decode_text_value(
                &data,
                charset,
                format,
                self.compressed_unicode,
            ))),
            // `CalcNumericColImpl.read`
            DataType::Numeric => {
                if data.is_empty() {
                    // apparently "null" values can be written as actual data
                    Ok(Value::Null)
                } else {
                    self.read_calc_numeric_value(&data, order)
                }
            }
            // `CalcColImpl.read`
            _ => {
                if data.is_empty() && !self.data_type.is_variable_length() {
                    // apparently "null" values can be written as actual data
                    return Ok(Value::Null);
                }
                self.read_plain_value(&data, order, charset, format, date_time_type)
            }
        }
    }

    /// `CalcNumericColImpl.readCalcNumericValue`: the wrapped numeric value has
    /// its own 2 byte total length and scale byte.
    fn read_calc_numeric_value(&self, data: &[u8], order: ByteOrder) -> Result<Value> {
        let mut buf = ByteBuf::wrap(data.to_vec(), order);
        let total_len = buf.get_i16()?;
        let mut num_len = if total_len > 0 {
            total_len as i32
        } else {
            buf.remaining() as i32
        } - 2;
        num_len = to_mul4(num_len).clamp(0, 16);
        let scale = buf.get_u8()? as i32;
        let negate = buf.get_u8()? != 0;
        let mut tmp = buf.get_bytes(num_len as usize)?;
        if order != ByteOrder::BigEndian {
            fix_calc_numeric_byte_order(&mut tmp);
        }
        Ok(Value::Decimal {
            unscaled: bytes_to_unscaled(&tmp, negate),
            scale,
        })
    }

    /// The `writeRealData` / `writeLongValue` implementations of
    /// `CalculatedColumnUtil`'s column classes: the real data is wrapped with
    /// the calculated value header.
    fn write_calculated_value(
        &self,
        value: &Value,
        order: ByteOrder,
        charset: Charset,
        format: &JetFormat,
        tz: &TimeZone,
        date_time_type: DateTimeType,
    ) -> Result<Vec<u8>> {
        let data = match self.data_type {
            // `CalcBooleanColImpl.writeRealData`
            DataType::Boolean => {
                if to_boolean_value(value) {
                    vec![0xFF]
                } else {
                    vec![0]
                }
            }
            // `CalcTextColImpl.writeRealData`
            DataType::Text => {
                let max = self.calculated_text_units(format);
                encode_text_value(
                    value,
                    0,
                    max,
                    false,
                    self.compressed_unicode,
                    charset,
                    format,
                )?
            }
            // `CalcMemoColImpl.writeLongValue` wraps the encoded text
            DataType::Memo => {
                let max = self.long_value_max_units(format);
                encode_text_value(
                    value,
                    0,
                    max,
                    false,
                    self.compressed_unicode,
                    charset,
                    format,
                )?
            }
            // `CalcNumericColImpl.writeRealData`
            DataType::Numeric => self.write_calc_numeric_value(value)?,
            // `CalcColImpl.writeRealData`
            _ => {
                self.write_fixed_length_field(value, order, charset, format, tz, date_time_type)?
            }
        };
        Ok(wrap_calculated_value(&data))
    }

    /// `CalcNumericColImpl.writeCalcNumericValue`: a 16 byte block holding the
    /// total length, the scale, the sign and the 12 byte unscaled value.
    fn write_calc_numeric_value(&self, value: &Value) -> Result<Vec<u8>> {
        let total_data_len = (CALC_EXTRA_DATA_LEN as i32 + 16 + 4).min(self.column_length as i32);
        let data_len = to_mul4(total_data_len - CALC_EXTRA_DATA_LEN as i32).max(0) as usize;
        if data_len < 4 {
            return Err(Error::Jackcess(
                "Calculated numeric column is too short".into(),
            ));
        }
        let (mut unscaled, scale) = value_as_decimal_parts(value)?;
        let signum = unscaled.signum();
        if unscaled < 0 {
            unscaled = -unscaled;
        }
        let max_scale = self.data_type.get_max_scale();
        if scale > max_scale {
            // Java's BigDecimal.setScale(maxScale) would throw
            return Err(Error::InvalidValue(
                "Numeric value out of range for calculated column".into(),
            ));
        }
        if decimal_digits(unscaled) > self.data_type.get_max_precision() {
            return Err(Error::InvalidValue(format!(
                "Numeric value is too big for specified precision {}",
                self.data_type.get_max_precision()
            )));
        }
        let mut bytes = unscaled_to_bytes_checked(unscaled, data_len - 4)?;
        fix_calc_numeric_byte_order(&mut bytes);
        let mut out = vec![0u8; data_len];
        out[0..2].copy_from_slice(&((data_len as i16) - 2).to_le_bytes());
        out[2] = scale as u8;
        out[3] = if signum < 0 { 0x80 } else { 0 };
        out[4..].copy_from_slice(&bytes);
        Ok(out)
    }
}

/// The Java `Column` object API which reads the column's context: `getTable`,
/// `getDatabase`, the row value accessors, the SQL type / length in units
/// getters and the version-history / complex-info links.
///
/// Java's `ColumnImpl` keeps an object reference to its owning `TableImpl`,
/// which is how `getTable`, `getDatabase` (`getTable().getDatabase()`),
/// `getVersionHistoryColumn` and `getComplexInfo` reach their context.  A Rust
/// [`Column`] is a plain value copied out of
/// [`crate::table::Table::columns`] (and from index/relationship lookups), so
/// the accessors which need that context take it as an explicit argument.
/// Every value-only accessor (`getSQLType`, `getLengthInUnits`, `isHyperlink`,
/// `getColumnIndex`, `getColumnId`, the row value accessors) keeps the Java
/// signature, and all of the existing field based APIs are unchanged.
impl Column {
    /// `Column.getTable()`: the table which owns this column.
    ///
    /// Java returns the reference stored in the column; the port takes the
    /// owning table as an argument and verifies that it really holds this
    /// column (same name, column number and column index), which is the
    /// invariant `ColumnImpl`'s constructor establishes.  An unrelated table is
    /// reported as an `IllegalArgumentException`.
    pub fn get_table<'a>(&self, table: &'a crate::table::Table) -> Result<&'a crate::table::Table> {
        let matches = table.columns.iter().any(|column| {
            (column.column_number == self.column_number)
                && (column.column_index == self.column_index)
                && column.name.eq_ignore_ascii_case(&self.name)
        });
        if matches {
            Ok(table)
        } else {
            Err(Error::IllegalArgument(format!(
                "Column {} is not a column of table {}",
                self.name, table.name
            )))
        }
    }

    /// `Column.getDatabase()`: the database of the owning table (Java:
    /// `getTable().getDatabase()`).
    ///
    /// The column keeps no back reference, so the caller supplies the database
    /// it read the owning table from.  Returning it keeps Java's chaining
    /// shape: `column.get_database(db).get_table(name)`.
    pub fn get_database<'a>(
        &self,
        db: &'a mut crate::database::Database,
    ) -> &'a mut crate::database::Database {
        db
    }

    /// `Column.getRowValue(Object[])`: this column's value in a row value
    /// array (indexed by [`Column::column_index`]).
    ///
    /// Java throws `ArrayIndexOutOfBoundsException` for a row which is too
    /// short; the port reports that as `None` (the element itself may still be
    /// [`Value::Null`], which is Java's null).
    pub fn get_row_value<'a>(&self, row: &'a [Value]) -> Option<&'a Value> {
        row.get(self.column_index)
    }

    /// `Column.setRowValue(Object[], Object)`: stores the value at this
    /// column's position of a row value array and returns it (Java returns the
    /// stored value, not the previous one).
    pub fn set_row_value(&self, row: &mut [Value], value: Value) -> Result<Value> {
        let row_len = row.len();
        let slot = row.get_mut(self.column_index).ok_or_else(|| {
            Error::IllegalArgument(format!(
                "Column {} (index {}) is outside the row of {} values",
                self.name, self.column_index, row_len
            ))
        })?;
        *slot = value.clone();
        Ok(value)
    }

    /// `Column.getRowValue(Map)`: this column's value in a row given as a
    /// column name map.
    ///
    /// The lookup is by the exact column name, like Java's
    /// `Map<String,?>.get(_name)`; a missing key is Java's null and therefore
    /// `None` here.  The port uses [`std::collections::BTreeMap`] because that
    /// is the map type of the row APIs in [`crate::cursor`].
    pub fn get_row_value_map<'a>(
        &self,
        row: &'a std::collections::BTreeMap<String, Value>,
    ) -> Option<&'a Value> {
        row.get(&self.name)
    }

    /// `Column.setRowValue(Map, Object)`: stores the value under this column's
    /// name and returns it.
    pub fn set_row_value_map(
        &self,
        row: &mut std::collections::BTreeMap<String, Value>,
        value: Value,
    ) -> Value {
        row.insert(self.name.clone(), value.clone());
        value
    }

    /// `Column.getSQLType()`: the JDBC SQL type of this column's
    /// [`DataType`] (`DataType.getSQLType`).
    ///
    /// Java throws a `JackcessException` for a data type without a SQL type
    /// (`GUID`, `BIG_BINARY`, `COMPLEX_TYPE`, `EXT_DATE_TIME` and the
    /// unsupported types); its message embeds `DataType`'s default
    /// `Object.toString` (an identity hash, so it cannot be compared or
    /// reproduced), the port reports the type name instead.
    pub fn get_sql_type(&self) -> Result<i32> {
        self.data_type.get_sql_type().ok_or_else(|| {
            Error::Jackcess(format!(
                "Unsupported data type: {}",
                self.data_type.get_name()
            ))
        })
    }

    /// `Column.getLengthInUnits()`: the column length in units, rounded up to
    /// the type's unit size (Java caches `calcLengthInUnits()`).
    ///
    /// Java asks its owning table for the [`JetFormat`]; a Rust column caches
    /// the value when the table is read, so the format is only needed as a
    /// fallback (a hand built column), exactly like
    /// [`Column::length_in_units`].  Java narrows the cached `int` to `short`,
    /// which this accessor mirrors.
    pub fn get_length_in_units(&self, format: &JetFormat) -> i16 {
        self.length_in_units(format) as i16
    }

    /// `Column.isHyperlink()`: `true` for a `MEMO` column whose definition
    /// carries the hyperlink flag bit.  Only memo columns can be hyperlinks
    /// (`MemoColumnImpl`); every other column reports `false`.
    pub fn is_hyperlink(&self) -> bool {
        (self.data_type == DataType::Memo) && ((self.flags & HYPERLINK_FLAG_MASK) != 0)
    }

    /// `Column.isSecurityIdentifier()`: whether the column holds a Windows
    /// security identifier (the `SECURITY_IDENTIFIER_FLAG_MASK` definition
    /// bit).
    pub fn is_security_identifier(&self) -> bool {
        (self.flags & SECURITY_IDENTIFIER_FLAG_MASK) != 0
    }

    /// `Column.getColumnIndex()`: the position of this column in the row value
    /// arrays.  Java returns an `int`; the port keeps the array index as a
    /// `usize`.
    pub fn get_column_index(&self) -> usize {
        self.column_index
    }

    /// `Column.getColumnId()`: the persisted column id (`short`).
    pub fn get_column_id(&self) -> i16 {
        self.column_id
    }

    /// `Column.getVersionHistoryColumn()`: the complex column which tracks the
    /// version history of this "append only" memo column, or `None` when the
    /// column has no version history.
    ///
    /// Java links the two columns while the table loads
    /// (`VersionHistoryColumnInfoImpl.postTableLoadInit`), reaching the owning
    /// table through the column's back reference.  The port resolves the link
    /// through the database, so the owning table name is passed in.
    pub fn get_version_history_column(
        &self,
        db: &mut crate::database::Database,
        table_name: &str,
    ) -> Result<Option<Column>> {
        // `ColumnImpl.getVersionHistoryColumn` is only overridden for memo
        // columns; every other column reports null
        if self.data_type != DataType::Memo {
            return Ok(None);
        }
        let Some(version_column) = db.version_history_column_name(table_name, &self.name)? else {
            return Ok(None);
        };
        let column = db
            .get_table(table_name)?
            .get_column(&version_column)
            .cloned();
        Ok(column)
    }

    /// `Column.getComplexInfo()`: the complex column info of a complex column
    /// (multi-value, attachment or version history), `None` for every other
    /// column.
    ///
    /// Java keeps the info on the column after the table load; the port
    /// resolves it through [`crate::database::Database::complex_info`] (which
    /// also follows a linked table to its linkee), so the owning table name is
    /// passed in.
    pub fn get_complex_info(
        &self,
        db: &mut crate::database::Database,
        table_name: &str,
    ) -> Result<Option<crate::complex::ComplexColumnInfo>> {
        // `ColumnImpl.getComplexInfo` is only overridden for the complex
        // column subclasses; every other column reports null
        if self.data_type != DataType::ComplexType {
            return Ok(None);
        }
        Ok(Some(db.complex_info(table_name, &self.name)?))
    }

    /// `Column.getProperties()`: this column's property map.
    ///
    /// Java caches `getTable().getPropertyMaps().get(getName())`, which
    /// creates an empty map for a column without properties; the port returns
    /// the map the database holds (an empty, named map when there is none).
    /// The owning table definition page identifies the table's property group.
    ///
    /// A multi-value column is special: `ComplexColumnImpl.getProperties`
    /// returns a `MultiValueColumnPropertyMap` which merges the primary
    /// column's properties with the properties of the flat table's value column
    /// (the primary map wins for a name which both hold).  The port returns the
    /// equivalent merged map.
    pub fn get_properties(
        &self,
        db: &mut crate::database::Database,
        table_page: i32,
    ) -> Result<crate::property::PropertyMap> {
        let maps = db.get_table_properties(table_page)?;
        let primary = maps.get(&self.name).cloned().unwrap_or_else(|| {
            crate::property::PropertyMap::with_name(
                self.name.clone(),
                crate::property::MAP_TYPE_COLUMN,
            )
        });
        if self.data_type != DataType::ComplexType {
            return Ok(primary);
        }
        let table_name = db.get_table_by_page(table_page)?.name.clone();
        let info = db.complex_info(&table_name, &self.name)?;
        if info.kind != crate::complex::ComplexDataType::MultiValue {
            return Ok(primary);
        }
        let Some(value_column) = info.value_column.clone() else {
            return Ok(primary);
        };
        let complex_maps = db.get_table_properties(info.flat_table_page)?;
        let complex = complex_maps.get(&value_column).cloned().unwrap_or_else(|| {
            crate::property::PropertyMap::with_name(
                value_column.clone(),
                crate::property::MAP_TYPE_COLUMN,
            )
        });
        let mut merged = primary;
        for prop in complex.iter() {
            if merged.get(&prop.name).is_none() {
                let flags = complex.flags(&prop.name).unwrap_or(0);
                merged.put_stored(prop.clone(), flags);
            }
        }
        Ok(merged)
    }

    // `Column.getColumnValidator()` / `Column.setColumnValidator()` are the
    // user ("external") validator slot of `ColumnImpl._validator`.  The
    // property driven part (`Required`, `AllowZeroLength`, `ValidationRule`)
    // wraps it in Java's `InternalColumnValidator`; the port applies those
    // checks in the table's row path, so the column stores the custom part.

    /// `Column.getColumnValidator()`: the column's user ("external") validator.
    ///
    /// Java stores the complete validator chain on the column
    /// (`ColumnImpl._validator`, an `InternalColumnValidator` wrapping the
    /// custom validator and the property driven checks).  The port keeps the
    /// custom part in [`Column::column_validator`] and applies the property
    /// driven checks through the owning table, so this accessor returns the
    /// stored validator, or the value preserving
    /// [`SimpleColumnValidator`](crate::validator::SimpleColumnValidator) when
    /// none was set (Java's default is the same validator inside the chain).
    pub fn get_column_validator(&self) -> crate::validator::ColumnValidatorHandle {
        match &self.column_validator {
            Some(validator) => validator.clone(),
            None => crate::validator::SimpleColumnValidator::handle(),
        }
    }

    /// `Column.setColumnValidator(ColumnValidator)`: sets the user ("external")
    /// validator, which the internal checks delegate to first
    /// (`InternalColumnValidator.validate`).
    ///
    /// Java accepts `null` to restore the default; the port spells that
    /// `None`.  The property driven checks of the column are unaffected, and
    /// the validator is consulted by the table's row path.
    pub fn set_column_validator(
        &mut self,
        validator: Option<crate::validator::ColumnValidatorHandle>,
    ) {
        self.column_validator = validator;
    }
}

/// The mutable column definition used while creating a table (`ColumnBuilder`).
#[derive(Clone, Debug)]
pub struct ColumnSpec {
    pub name: String,
    pub data_type: DataType,
    pub length: Option<i16>,
    pub precision: Option<u8>,
    pub scale: Option<u8>,
    pub auto_number: bool,
    pub compressed_unicode: bool,
    pub hyperlink: bool,
    pub calculated: bool,
    pub sort_order: Option<SortOrder>,
    pub column_number: i16,
    pub column_id: i16,
    pub append_only: bool,
    /// the complex column this builder declares, if any (`ComplexColumnDesc`)
    pub complex_desc: Option<crate::complex::ComplexColumnDesc>,
    /// extra definition flags (`TableMutator.ColumnState.setExtraFlags`), e.g.
    /// the complex foreign key bit of a flat table's parent id column
    pub extra_flags: u8,
    /// `ColumnBuilder.putProperty`: properties the table creator stores in the
    /// column's property map after the table has been created
    pub properties: Vec<crate::property::Property>,
}

impl ColumnSpec {
    pub fn new(name: impl Into<String>, data_type: DataType) -> ColumnSpec {
        ColumnSpec {
            name: name.into(),
            data_type,
            length: None,
            precision: None,
            scale: None,
            auto_number: false,
            compressed_unicode: false,
            hyperlink: false,
            calculated: false,
            sort_order: None,
            column_number: 0,
            column_id: 0,
            append_only: false,
            complex_desc: None,
            extra_flags: 0,
            properties: Vec::new(),
        }
    }

    /// `ColumnBuilder.setMultiValue`: makes this a multi-value complex column
    /// which holds values of the given type.  A row source (from
    /// [`ColumnSpec::set_value_list`] or [`ColumnSpec::set_row_source`]) is
    /// still needed.
    pub fn set_multi_value(mut self, value_type: DataType) -> ColumnSpec {
        self.data_type = DataType::ComplexType;
        self.auto_number = true;
        self.complex_desc = Some(crate::complex::ComplexColumnDesc::multi_value(value_type));
        self
    }

    /// `ColumnBuilder.setValueList`: the values the multi-value column offers,
    /// written as the `RowSource` property of its value column.
    pub fn set_value_list(mut self, values: &[&str]) -> ColumnSpec {
        let desc = self
            .complex_desc
            .as_mut()
            .filter(|d| d.kind == crate::complex::ComplexDataType::MultiValue)
            .expect("set_value_list needs a multi-value column (set_multi_value)");
        let mut row_source = String::new();
        for value in values {
            if !row_source.is_empty() {
                row_source.push(';');
            }
            row_source.push('"');
            row_source.push_str(&value.replace('"', "\"\""));
            row_source.push('"');
        }
        desc.set_row_source("Value List", row_source, 1);
        self
    }

    /// `ColumnBuilder.setRowSource`: a table, query or SELECT statement as the
    /// source of the values the multi-value column offers.
    pub fn set_row_source(mut self, table_or_query: &str, column_count: i32) -> ColumnSpec {
        let desc = self
            .complex_desc
            .as_mut()
            .filter(|d| d.kind == crate::complex::ComplexDataType::MultiValue)
            .expect("set_row_source needs a multi-value column (set_multi_value)");
        desc.set_row_source("Table/Query", table_or_query, column_count);
        self
    }

    /// `ColumnBuilder.setAttachment`: makes this an attachment column.
    pub fn set_attachment(mut self) -> ColumnSpec {
        self.data_type = DataType::ComplexType;
        self.auto_number = true;
        self.complex_desc = Some(crate::complex::ComplexColumnDesc::attachment());
        self
    }

    /// `ColumnBuilder.setAppendOnly`: makes a MEMO column keep the history of
    /// its values.  The table creator adds the version history complex column
    /// beside the memo column (see [`crate::complex::ComplexDataType::
    /// VersionHistory`]) and marks both the memo column and its table with the
    /// `AppendOnly` property.
    pub fn set_append_only(mut self, append_only: bool) -> ColumnSpec {
        self.append_only = append_only;
        if append_only {
            let prop = crate::property::Property::new(
                crate::property::APPEND_ONLY_PROP.to_string(),
                DataType::Boolean,
                Value::Bool(true),
            );
            crate::property::put_builder_property(&mut self.properties, prop);
        } else {
            // the property tells ms access the column keeps a history, so it
            // cannot outlive the column which holds one
            self.properties.retain(|p| {
                !p.name
                    .eq_ignore_ascii_case(crate::property::APPEND_ONLY_PROP)
            });
        }
        self
    }

    pub fn with_length(mut self, length: i16) -> ColumnSpec {
        self.length = Some(length);
        self
    }

    /// `ColumnBuilder.setCalculated`.
    pub fn set_calculated(mut self, calculated: bool) -> ColumnSpec {
        self.calculated = calculated;
        self
    }

    /// `ColumnBuilder.setCalculatedInfo`: the convenience method which sets the
    /// calculated flag and stores the `Expression` and `ResultType` properties
    /// (in the order the Java builder adds them).
    pub fn set_calculated_info(mut self, expression: impl Into<String>) -> ColumnSpec {
        self.calculated = true;
        let expr = Property::new(
            crate::property::EXPRESSION_PROP.to_string(),
            DataType::Memo,
            Value::Text(expression.into()),
        );
        crate::property::put_builder_property(&mut self.properties, expr);
        let result_type = Property::new(
            crate::property::RESULT_TYPE_PROP.to_string(),
            DataType::Byte,
            Value::Byte(self.data_type.value()),
        );
        crate::property::put_builder_property(&mut self.properties, result_type);
        self
    }

    /// The value of a builder property (case insensitive), if it is set.
    fn property_value(&self, name: &str) -> Option<&Value> {
        self.properties
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
            .map(|p| &p.value)
    }

    /// `ColumnBuilder.validate`'s calculated column checks: the data type must
    /// be one the database format supports, an expression is required, and the
    /// result type property is filled in when it is missing (like the Java
    /// builder does).
    pub fn validate_calculated(&mut self, format: &JetFormat) -> Result<()> {
        if !self.calculated {
            return Ok(());
        }
        if !format.is_supported_calculated_data_type(self.data_type) {
            return Err(Error::IllegalArgument(format!(
                "Database format {} does not support calculated type {}(Column={})",
                format.name,
                self.data_type.get_name(),
                self.name
            )));
        }
        if self
            .property_value(crate::property::EXPRESSION_PROP)
            .is_none()
        {
            return Err(Error::IllegalArgument(format!(
                "No expression provided for calculated type {}(Column={})",
                self.data_type.get_name(),
                self.name
            )));
        }
        if self
            .property_value(crate::property::RESULT_TYPE_PROP)
            .is_none()
        {
            let prop = Property::new(
                crate::property::RESULT_TYPE_PROP.to_string(),
                DataType::Byte,
                Value::Byte(self.data_type.value()),
            );
            crate::property::put_builder_property(&mut self.properties, prop);
        }
        Ok(())
    }

    pub fn with_auto_number(mut self, v: bool) -> ColumnSpec {
        self.auto_number = v;
        self
    }

    /// `ColumnBuilder.putProperty(name, value)`: a property of the new column
    /// (stored in the column's property map by the table creator).
    pub fn put_property(mut self, name: impl Into<String>, value: Value) -> Result<ColumnSpec> {
        let name = name.into();
        let prop = crate::property::create_property(&name, value)?;
        crate::property::put_builder_property(&mut self.properties, prop);
        Ok(self)
    }

    /// `ColumnBuilder.putProperty(name, type, value)`.
    pub fn put_property_typed(
        mut self,
        name: impl Into<String>,
        data_type: DataType,
        value: Value,
    ) -> ColumnSpec {
        let prop = crate::property::Property::new(name.into(), data_type, value);
        crate::property::put_builder_property(&mut self.properties, prop);
        self
    }

    pub fn with_compressed_unicode(mut self, v: bool) -> ColumnSpec {
        self.compressed_unicode = v;
        self
    }

    pub fn with_precision(mut self, v: u8) -> ColumnSpec {
        self.precision = Some(v);
        self
    }

    pub fn with_scale(mut self, v: u8) -> ColumnSpec {
        self.scale = Some(v);
        self
    }

    pub fn get_length(&self) -> i16 {
        self.length.unwrap_or_else(|| {
            if !self.data_type.is_variable_length() {
                self.data_type.get_fixed_size() as i16
            } else {
                self.data_type.get_default_size() as i16
            }
        })
    }

    pub fn get_precision(&self) -> u8 {
        self.precision
            .unwrap_or(self.data_type.get_default_precision() as u8)
    }

    pub fn get_scale(&self) -> u8 {
        self.scale
            .unwrap_or(self.data_type.get_default_scale() as u8)
    }

    pub fn is_variable_length(&self) -> bool {
        self.data_type.is_variable_length() || self.calculated
    }

    pub fn fixed_data_size(&self) -> i32 {
        self.data_type
            .get_fixed_size_with_length(Some(self.get_length()))
    }

    pub fn store_in_null_mask(&self) -> bool {
        self.data_type == DataType::Boolean
    }

    pub fn is_textual(&self) -> bool {
        self.data_type.is_textual()
    }

    pub fn get_column_bit_flags(&self) -> u8 {
        let mut flags = UPDATABLE_FLAG_MASK;
        if !self.is_variable_length() {
            flags |= FIXED_LEN_FLAG_MASK;
        }
        if self.auto_number {
            flags |= match self.data_type {
                DataType::Long | DataType::ComplexType => AUTO_NUMBER_FLAG_MASK,
                DataType::Guid => AUTO_NUMBER_GUID_FLAG_MASK,
                _ => 0,
            };
        }
        if self.hyperlink {
            flags |= HYPERLINK_FLAG_MASK;
        }
        flags
    }
}

/// `TableMutator.ColumnOffsets`
#[derive(Clone, Copy, Debug, Default)]
pub struct ColumnOffsets {
    pub fixed_offset: i16,
    pub var_offset: i16,
    pub long_var_offset: i16,
}

impl ColumnOffsets {
    pub fn set(&mut self, fixed: i16, var: i16, long_var: i16) {
        self.fixed_offset = fixed;
        self.var_offset = var;
        self.long_var_offset = long_var;
    }

    pub fn get_next_variable_offset(&mut self, col: &ColumnSpec) -> i16 {
        if !col.is_variable_length() {
            return self.var_offset;
        }
        if !col.data_type.is_long_value() {
            let r = self.var_offset;
            self.var_offset += 1;
            r
        } else {
            let r = self.long_var_offset;
            self.long_var_offset += 1;
            r
        }
    }

    pub fn get_next_fixed_offset(&mut self, col: &ColumnSpec) -> i16 {
        if col.store_in_null_mask() {
            return 0;
        }
        let offset = self.fixed_offset;
        self.fixed_offset += col.fixed_data_size() as i16;
        offset
    }
}

/// `TableMutator.ColumnState`
#[derive(Clone, Copy, Debug, Default)]
pub struct ColumnState {
    pub umap_owned_row_number: u8,
    pub umap_free_row_number: u8,
    pub umap_page_number: i32,
    pub complex_id: i32,
    pub extra_flags: u8,
}

pub fn calculate_name_length(name: &str) -> usize {
    // 2 bytes per char + 2 bytes for the length
    2 * name.encode_utf16().count() + 2
}

/// `TableImpl.writeName`
pub fn write_name(buffer: &mut ByteBuf, name: &str, charset: Charset) -> Result<()> {
    let bytes = encode_uncompressed_text(name, charset);
    buffer.put_u16(bytes.len() as u16)?;
    buffer.put_bytes(&bytes)?;
    Ok(())
}

/// `ColumnImpl.writeDefinitions`
pub fn write_definitions(
    specs: &[ColumnSpec],
    offsets: &mut ColumnOffsets,
    states: &[ColumnState],
    buffer: &mut ByteBuf,
    format: &JetFormat,
) -> Result<()> {
    let long_var_offset = count_non_long_variable_length(specs);
    offsets.set(0, 0, long_var_offset);
    for (i, col) in specs.iter().enumerate() {
        write_definition(col, states.get(i), offsets, buffer, format)?;
    }
    for col in specs {
        write_name(buffer, &col.name, format.charset)?;
    }
    Ok(())
}

pub fn count_non_long_variable_length(specs: &[ColumnSpec]) -> i16 {
    let mut rtn = 0;
    for col in specs {
        if col.is_variable_length() && !col.data_type.is_long_value() {
            rtn += 1;
        }
    }
    rtn
}

/// `ColumnImpl.countVariableLength`
pub fn count_variable_length(specs: &[ColumnSpec]) -> i16 {
    specs.iter().filter(|c| c.is_variable_length()).count() as i16
}

/// `ColumnImpl.writeDefinition`
pub fn write_definition(
    col: &ColumnSpec,
    state: Option<&ColumnState>,
    offsets: &mut ColumnOffsets,
    buffer: &mut ByteBuf,
    format: &JetFormat,
) -> Result<()> {
    buffer.put_u8(col.data_type.value())?;
    buffer.put_i32(crate::table::MAGIC_TABLE_NUMBER)?;
    buffer.put_i16(col.column_number)?;
    let var_offset = offsets.get_next_variable_offset(col);
    buffer.put_i16(var_offset)?;
    buffer.put_i16(col.column_id)?;

    if col.data_type.is_textual() {
        write_sort_order(
            buffer,
            col.sort_order
                .as_ref()
                .unwrap_or(&format.default_sort_order),
            format,
        )?;
    } else if col.data_type == DataType::ComplexType {
        buffer.put_i32(state.map(|s| s.complex_id).unwrap_or(0))?;
    } else {
        if col.data_type.has_scale_precision() && !col.calculated {
            buffer.put_u8(col.get_precision())?;
            buffer.put_u8(col.get_scale())?;
        } else {
            buffer.put_u8(0)?;
            buffer.put_u8(0)?;
        }
        buffer.put_i16(0)?;
    }

    buffer.put_u8(col.get_column_bit_flags())?;

    let mut ext_flags = state.map(|s| s.extra_flags).unwrap_or(0);
    if col
        .complex_desc
        .as_ref()
        .map(|d| d.kind == crate::complex::ComplexDataType::VersionHistory)
        .unwrap_or(false)
    {
        // this bit tells ms access the column keeps the history of a memo
        // column rather than holding values of its own
        ext_flags |= VERSION_HISTORY_EXT_FLAG_MASK;
    }
    if col.calculated {
        ext_flags |= CALCULATED_EXT_FLAG_MASK;
    } else if col.compressed_unicode {
        ext_flags |= COMPRESSED_UNICODE_EXT_FLAG_MASK;
    }
    buffer.put_u8(ext_flags)?;

    buffer.put_i32(0)?;

    if col.is_variable_length() {
        buffer.put_i16(0)?;
    } else {
        let off = offsets.get_next_fixed_offset(col);
        buffer.put_i16(off)?;
    }

    if !col.data_type.is_long_value() {
        let mut length = col.get_length();
        if col.calculated {
            // calced columns have additional value overhead
            if !col.data_type.is_variable_length() || col.data_type.has_scale_precision() {
                length = CALC_FIXED_FIELD_LEN;
            } else {
                length += CALC_EXTRA_DATA_LEN as i16;
            }
        }
        buffer.put_i16(length)?;
    } else {
        buffer.put_i16(0)?;
    }
    Ok(())
}

pub fn write_col_usage_map_definition(
    col: &ColumnSpec,
    state: &ColumnState,
    buffer: &mut ByteBuf,
) -> Result<()> {
    buffer.put_i16(col.column_number)?;
    buffer.put_u8(state.umap_owned_row_number)?;
    crate::byteutil::put3_byte_int(buffer, state.umap_page_number)?;
    buffer.put_u8(state.umap_free_row_number)?;
    crate::byteutil::put3_byte_int(buffer, state.umap_page_number)?;
    Ok(())
}

pub fn read_sort_order(buffer: &ByteBuf, position: usize, format: &JetFormat) -> Result<SortOrder> {
    let value = buffer.get_i16_at(position)?;
    if value == 0 {
        return Ok(format.default_sort_order);
    }
    let mut variant = 0i16;
    let mut version = format.default_sort_order.version;
    if format.size_sort_order == 4 {
        variant = buffer.get_u8_at(position + 2)? as i16;
        version = buffer.get_u8_at(position + 3)? as i8 as i16;
    }
    if value == 1033 && variant == 0 {
        if version == SortOrder::GENERAL.version {
            return Ok(SortOrder::GENERAL);
        }
        if version == SortOrder::GENERAL_LEGACY.version {
            return Ok(SortOrder::GENERAL_LEGACY);
        }
        if version == SortOrder::GENERAL_97.version {
            return Ok(SortOrder::GENERAL_97);
        }
    }
    Ok(SortOrder {
        value,
        variant,
        version,
    })
}

pub fn write_sort_order(
    buffer: &mut ByteBuf,
    sort_order: &SortOrder,
    format: &JetFormat,
) -> Result<()> {
    buffer.put_i16(sort_order.value)?;
    if format.size_sort_order == 4 {
        buffer.put_u8(sort_order.variant as u8)?;
        buffer.put_u8(sort_order.version as u8)?;
    }
    Ok(())
}

pub fn read_code_page(buffer: &ByteBuf, offset: usize, format: &JetFormat) -> Result<i16> {
    if format.offset_column_code_page >= 0 {
        buffer.get_i16_at(offset + format.offset_column_code_page as usize)
    } else {
        Ok(0)
    }
}

/// `ColumnImpl.decodeTextValue` - decodes compressed/uncompressed text.
pub fn decode_text_value(
    data: &[u8],
    charset: Charset,
    format: &JetFormat,
    _compressed_unicode: bool,
) -> String {
    let is_compressed = !format.code_page_charset
        && data.len() > 1
        && data[0] == TEXT_COMPRESSION_HEADER[0]
        && data[1] == TEXT_COMPRESSION_HEADER[1];

    if is_compressed {
        let mut text = String::new();
        let mut data_start = 2;
        let mut data_end = data_start;
        let mut in_compressed = true;
        while data_end < data.len() {
            if data[data_end] == 0 {
                decode_text_segment(
                    data,
                    data_start,
                    data_end,
                    in_compressed,
                    charset,
                    &mut text,
                );
                in_compressed = !in_compressed;
                data_end += 1;
                data_start = data_end;
            } else {
                data_end += if in_compressed { 1 } else { 2 };
            }
        }
        decode_text_segment(
            data,
            data_start,
            data.len(),
            in_compressed,
            charset,
            &mut text,
        );
        return text;
    }

    decode_uncompressed_text(data, 0, data.len(), charset)
}

fn decode_text_segment(
    data: &[u8],
    data_start: usize,
    data_end: usize,
    in_compressed: bool,
    charset: Charset,
    out: &mut String,
) {
    if data_end <= data_start {
        return;
    }
    if in_compressed {
        let mut tmp = vec![0u8; (data_end - data_start) * 2];
        for (i, &b) in data[data_start..data_end].iter().enumerate() {
            tmp[i * 2] = b;
        }
        out.push_str(&decode_uncompressed_text(&tmp, 0, tmp.len(), charset));
    } else {
        out.push_str(&decode_uncompressed_text(
            data,
            data_start,
            data_end - data_start,
            charset,
        ));
    }
}

pub fn decode_uncompressed_text(
    data: &[u8],
    start: usize,
    length: usize,
    charset: Charset,
) -> String {
    let units = charset.decode(&data[start..start + length]);
    String::from_utf16_lossy(&units)
}

pub fn encode_uncompressed_text(text: &str, charset: Charset) -> Vec<u8> {
    // [`crate::format::Charset::encode`] implements `Charset.encode(String)`
    // for every code page: UTF-16LE/UTF-8, the single byte windows/ISO/DOS/IBM
    // pages and the East Asian multi byte pages, writing '?' for a character
    // the charset cannot encode (Java's REPLACE action)
    charset.encode(text)
}

/// `ColumnImpl.encodeTextValue`
#[allow(clippy::too_many_arguments)]
pub fn encode_text_value(
    value: &Value,
    min_chars: i32,
    max_chars: i32,
    force_uncompressed: bool,
    compressed_unicode: bool,
    charset: Charset,
    format: &JetFormat,
) -> Result<Vec<u8>> {
    let text = to_char_sequence(value)?;
    let char_len = text.encode_utf16().count() as i32;
    if char_len > max_chars || char_len < min_chars {
        return Err(Error::Jackcess(format!(
            "Text is wrong length for column, max {}, min {}, got {}",
            max_chars, min_chars, char_len
        )));
    }
    if !force_uncompressed
        && compressed_unicode
        && char_len as usize <= format.max_compressed_unicode_size
        && is_unicode_compressible(&text)
    {
        let mut out = Vec::with_capacity(2 + text.len());
        out.extend_from_slice(&TEXT_COMPRESSION_HEADER);
        for u in text.encode_utf16() {
            out.push(u as u8);
        }
        return Ok(out);
    }
    Ok(encode_uncompressed_text(&text, charset))
}

fn is_unicode_compressible(text: &str) -> bool {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= TEXT_COMPRESSION_HEADER.len() {
        return false;
    }
    for u in units {
        let c = u as u32;
        if !(MIN_COMPRESS_CHAR..=MAX_COMPRESS_CHAR).contains(&c) {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// value conversions
// ---------------------------------------------------------------------------

pub fn to_boolean_value(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Text(s) => s.eq_ignore_ascii_case("true") || s == "1",
        _ => value.as_i64().unwrap_or(0) != 0,
    }
}

fn boolean_to_integer(value: &Value) -> Value {
    match value {
        Value::Bool(b) => Value::Byte(if *b { 1 } else { 0 }),
        other => other.clone(),
    }
}

pub fn to_number(value: &Value) -> Result<f64> {
    match value {
        Value::Bool(b) => Ok(if *b { 1.0 } else { -0.0 }),
        Value::Byte(v) => Ok((*v as i8) as f64),
        Value::Int(v) => Ok(*v as f64),
        Value::Long(v) => Ok(*v as f64),
        Value::BigInt(v) => Ok(*v as f64),
        Value::Float(v) => Ok(*v as f64),
        Value::Double(v) => Ok(*v),
        Value::Decimal { unscaled, scale } => Ok(*unscaled as f64 / 10f64.powi(*scale)),
        Value::Text(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|e| Error::Jackcess(format!("invalid number {s}: {e}"))),
        Value::Null => Ok(0.0),
        _ => Err(Error::Jackcess("cannot convert value to number".into())),
    }
}

pub fn to_decimal(value: &Value) -> Result<f64> {
    to_number(value)
}

pub fn to_char_sequence(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::Text(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(if *b { "true".into() } else { "false".into() }),
        Value::Byte(_)
        | Value::Int(_)
        | Value::Long(_)
        | Value::BigInt(_)
        | Value::Float(_)
        | Value::Double(_)
        | Value::Decimal { .. } => Ok(format_number(value)),
        Value::DateTime(ldt) => Ok(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            ldt.year, ldt.month, ldt.day, ldt.hour, ldt.minute, ldt.second
        )),
        _ => Err(Error::Jackcess("cannot convert value to text".into())),
    }
}

fn format_number(v: &Value) -> String {
    match v {
        Value::Byte(x) => x.to_string(),
        Value::Int(x) => x.to_string(),
        Value::Long(x) => x.to_string(),
        Value::BigInt(x) => x.to_string(),
        Value::Float(x) => x.to_string(),
        Value::Double(x) => x.to_string(),
        Value::Decimal { unscaled, scale } => {
            let s = *unscaled as f64 / 10f64.powi(*scale);
            s.to_string()
        }
        _ => String::new(),
    }
}

pub fn to_byte_array(value: &Value) -> Result<Vec<u8>> {
    match value {
        Value::Bytes(b) => Ok(b.clone()),
        Value::Text(s) => Ok(s.as_bytes().to_vec()),
        Value::Null => Ok(Vec::new()),
        _ => Err(Error::Jackcess("cannot convert value to bytes".into())),
    }
}

// ---------------------------------------------------------------------------
// default values (`ColumnImpl.generateDefaultValue` /
// `ColDefaultValueEvalContext` / `Expressionator` DEFAULT_VALUE literals)
// ---------------------------------------------------------------------------

/// `Expressionator`'s DEFAULT_VALUE literal handling, restricted to the forms
/// the port can reproduce byte for byte.
///
/// * A textual column takes the whole (trimmed) property as literal text
///   unless it starts with a quote character (`"` or `'`), exactly like
///   `ColDefaultValueEvalContext.allowLiteralDefaultValue` with
///   `Expressionator.isLiteralDefaultValue`.
/// * A quoted string literal is parsed with Access' doubled-quote escape.
/// * A numeric/BOOLEAN column takes a numeric literal (with an optional sign
///   and exponent) plus the `True`/`False`/`Yes`/`No`/`On`/`Off`/`Null`
///   constants.  The literal is converted with the `Value.Type` the Java
///   expression wrapper uses for the column type, so e.g. `1.5` in a LONG
///   column becomes `2` (round half even).
/// * A default starting with `=` is full expression mode in the Java library;
///   the port reports it as unsupported rather than writing a different value.
fn eval_default_value(col: &Column, expr: &str) -> Result<Value> {
    let data_type = col.data_type;
    let col_name = col.name.as_str();
    let expr = expr.trim();
    if expr.is_empty() {
        // `getTrimmedStringProperty` would have treated it as no property
        return Ok(Value::Null);
    }
    if expr.starts_with('=') {
        return Err(Error::Unsupported(format!(
            "DefaultValue expression for column '{}' is not supported: {}",
            col_name, expr
        )));
    }
    match data_type {
        DataType::Text | DataType::Memo => {
            if expr.starts_with('"') || expr.starts_with('\'') {
                Ok(Value::Text(parse_quoted_default(expr, col_name)?))
            } else if expr.starts_with('#') || expr.starts_with('[') {
                // Access date literal / object reference, which the Java
                // expression engine evaluates instead of taking as text
                Err(Error::Unsupported(format!(
                    "DefaultValue for column '{}' is an Access expression the port does not support: {}",
                    col_name, expr
                )))
            } else {
                Ok(Value::Text(expr.to_string()))
            }
        }
        DataType::Boolean
        | DataType::Byte
        | DataType::Int
        | DataType::Long
        | DataType::Money
        | DataType::Float
        | DataType::Double
        | DataType::Numeric
        | DataType::BigInt => parse_numeric_default(expr, col_name)?.to_value(col),
        DataType::ShortDateTime => parse_short_date_time_default(expr, col_name),
        DataType::ExtDateTime => {
            let text = expr
                .strip_prefix('#')
                .and_then(|rest| rest.strip_suffix('#'))
                .ok_or_else(|| {
                    Error::Unsupported(format!(
                        "DefaultValue for {} column '{}' is not supported: {}",
                        data_type.get_name(),
                        col_name,
                        expr
                    ))
                })?;
            Ok(Value::DateTime(parse_date_literal_text(text, col_name)?))
        }
        _ => Err(Error::Unsupported(format!(
            "DefaultValue for {} column '{}' is not supported: {}",
            data_type.get_name(),
            col_name,
            expr
        ))),
    }
}

/// A default value for a `SHORT_DATE_TIME` column
/// (`ColumnImpl.generateDefaultValue` with the `DateTimeFactory`'s
/// `toInternalValue`): an Access date literal (`#2020-03-15 12:30:45#`) or a
/// numeric literal, which Java treats as a `java.util.Date` epoch value.
///
/// The returned value is a wall clock [`Value::DateTime`] for the date literal
/// (the row writer's `DateTimeFactory` conversion then applies the database
/// zone exactly like Java's `new Date(ldt.atZone(zone).toInstant())` does) and
/// a [`Value::BigInt`] instant for the numeric form.
fn parse_short_date_time_default(expr: &str, col_name: &str) -> Result<Value> {
    if let Some(rest) = expr.strip_prefix('#') {
        let text = rest.strip_suffix('#').ok_or_else(|| {
            Error::Jackcess(format!(
                "Invalid DefaultValue for column '{}': missing closing '#' in {}",
                col_name, expr
            ))
        })?;
        return Ok(Value::DateTime(parse_date_literal_text(text, col_name)?));
    }
    if expr.starts_with('[') {
        // an object reference (`[Name]`), which the Java expression engine
        // evaluates instead of taking it as a literal
        return Err(Error::Unsupported(format!(
            "DefaultValue for column '{}' is an Access expression the port does not support: {}",
            col_name, expr
        )));
    }
    match parse_numeric_default(expr, col_name)? {
        NumericDefault::Null => Ok(Value::Null),
        other => Ok(Value::BigInt(other.as_big_int()?)),
    }
}

/// Parses the text between the `#` of an Access date literal, in the ISO forms
/// the port supports: `yyyy-mm-dd`, `hh:mm[:ss[.fff]]` and
/// `yyyy-mm-dd[ T]hh:mm[:ss[.fff]]`.  A time-only literal uses the OLE base
/// date (`ColumnImpl.BASE_LD`, 1899-12-30), like Java's `DateTimeParser`.
fn parse_date_literal_text(text: &str, col_name: &str) -> Result<LocalDateTime> {
    let invalid = || {
        Error::Unsupported(format!(
            "DefaultValue date literal '{}' for column '{}' is not supported \
             (only the ISO forms yyyy-mm-dd, hh:mm[:ss] and yyyy-mm-dd hh:mm[:ss])",
            text, col_name
        ))
    };
    let text = text.trim();
    let (date_part, time_part) = match text.find([' ', 'T']) {
        Some(index) => (&text[..index], Some(text[index + 1..].trim())),
        None if text.contains(':') => ("", Some(text)),
        None => (text, None),
    };
    let (year, month, day) = if date_part.is_empty() {
        (1899, 12, 30)
    } else {
        let fields: Vec<&str> = date_part.split('-').collect();
        if fields.len() != 3
            || fields[0].len() != 4
            || !fields
                .iter()
                .all(|f| !f.is_empty() && f.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(invalid());
        }
        let year: i32 = fields[0].parse().map_err(|_| invalid())?;
        let month: u32 = fields[1].parse().map_err(|_| invalid())?;
        let day: u32 = fields[2].parse().map_err(|_| invalid())?;
        if month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
            return Err(invalid());
        }
        (year, month, day)
    };
    let (mut hour, mut minute, mut second, mut nano) = (0u32, 0u32, 0u32, 0u32);
    if let Some(time) = time_part {
        let (clock, fraction) = match time.split_once('.') {
            Some((clock, fraction)) => (clock, Some(fraction)),
            None => (time, None),
        };
        let fields: Vec<&str> = clock.split(':').collect();
        if fields.len() < 2
            || fields.len() > 3
            || !fields
                .iter()
                .all(|f| !f.is_empty() && f.len() <= 2 && f.bytes().all(|b| b.is_ascii_digit()))
        {
            return Err(invalid());
        }
        hour = fields[0].parse().map_err(|_| invalid())?;
        minute = fields[1].parse().map_err(|_| invalid())?;
        if fields.len() == 3 {
            second = fields[2].parse().map_err(|_| invalid())?;
        }
        if let Some(fraction) = fraction {
            if fraction.is_empty()
                || fraction.len() > 9
                || !fraction.bytes().all(|b| b.is_ascii_digit())
            {
                return Err(invalid());
            }
            let mut digits = fraction.to_string();
            while digits.len() < 9 {
                digits.push('0');
            }
            nano = digits.parse().map_err(|_| invalid())?;
        }
        if hour > 23 || minute > 59 || second > 59 {
            return Err(invalid());
        }
    }
    Ok(LocalDateTime::new(
        year, month, day, hour, minute, second, nano,
    ))
}

/// `LocalDate.lengthOfMonth` for the proleptic Gregorian calendar.
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Parses an Access quoted string literal (`ExpressionTokenizer
/// .parseQuotedString`): the opening quote character determines the closing
/// one, and a doubled quote is an escaped quote.  The literal must be the
/// whole expression.
fn parse_quoted_default(expr: &str, col_name: &str) -> Result<String> {
    let mut chars = expr.chars();
    let quote = chars.next().unwrap();
    let mut out = String::new();
    let mut closed = false;
    while let Some(c) = chars.next() {
        if c == quote {
            if chars.clone().next() == Some(quote) {
                chars.next();
                out.push(quote);
            } else {
                closed = true;
                break;
            }
        } else {
            out.push(c);
        }
    }
    if !closed {
        return Err(Error::Jackcess(format!(
            "Invalid DefaultValue for column '{}': missing closing '{}' in {}",
            col_name, quote, expr
        )));
    }
    let rest: String = chars.collect();
    if !rest.trim().is_empty() {
        // `"abc" & "def"` and friends are expressions, not a literal
        return Err(Error::Unsupported(format!(
            "DefaultValue for column '{}' is an Access expression the port does not support: {}",
            col_name, expr
        )));
    }
    Ok(out)
}

/// The constants and numeric literals `Expressionator` accepts for a default
/// value expression, before the column's result type is applied.
#[derive(Clone, Debug)]
enum NumericDefault {
    /// the `Null` constant (any result type evaluates to null)
    Null,
    /// a `LongValue` (`Integer`), from an integer literal or a boolean constant
    Int(i32),
    /// a `BigDecimal` literal, with the token text for the correctly rounded
    /// `double` conversion
    Number { text: String, dec: DecimalLiteral },
}

impl NumericDefault {
    /// Applies the `Value.Type` conversion `Expressionator.BaseExprWrapper
    /// .evalValue` uses for the column type, then `ColumnImpl.toInternalValue`.
    fn to_value(&self, col: &Column) -> Result<Value> {
        let data_type = col.data_type;
        if matches!(self, NumericDefault::Null) {
            return Ok(Value::Null);
        }
        match data_type {
            DataType::Boolean => Ok(Value::Bool(self.as_long_int()? != 0)),
            DataType::Byte => Ok(Value::Byte(self.as_long_int()? as i8 as u8)),
            DataType::Int => Ok(Value::Int(self.as_long_int()? as i16)),
            DataType::Long => Ok(Value::Long(self.as_long_int()?)),
            DataType::BigInt => Ok(Value::BigInt(self.as_big_int()?)),
            DataType::Money => {
                // the Java expression wrapper converts the literal to a
                // `Double`, and `writeCurrencyValue` stores
                // `new BigDecimal(double).setScale(4)`, which raises an
                // ArithmeticException when the exact binary value needs more
                // than four decimal places (e.g. 0.1)
                let value = self.as_f64();
                let places = exact_decimal_places(value).ok_or_else(|| {
                    Error::Unsupported(format!(
                        "DefaultValue for MONEY column '{}' is not finite",
                        col.name
                    ))
                })?;
                let scaled = value * 10000.0;
                if places > 4 || !scaled.is_finite() || scaled.abs() > 9007199254740992.0 {
                    return Err(Error::Unsupported(format!(
                        "DefaultValue {} for MONEY column '{}' cannot be stored exactly \
                         (jackcess raises an ArithmeticException or writes a value the port \
                         cannot reproduce)",
                        self.describe(),
                        col.name
                    )));
                }
                Ok(Value::Double(value))
            }
            DataType::Float => Ok(Value::Float(self.as_f64() as f32)),
            DataType::Double => Ok(Value::Double(self.as_f64())),
            DataType::Numeric => match self {
                NumericDefault::Int(i) => Ok(Value::Decimal {
                    unscaled: *i as i128,
                    scale: 0,
                }),
                NumericDefault::Number { dec, .. } => {
                    // `writeNumericValue` calls `setScale(getScale())`, which
                    // raises an ArithmeticException instead of rounding
                    if !fits_scale(dec, col.scale as i32) {
                        return Err(Error::Unsupported(format!(
                            "DefaultValue {} for NUMERIC column '{}' needs more decimal places \
                             than the column stores (jackcess raises an ArithmeticException)",
                            dec.display(),
                            col.name
                        )));
                    }
                    Ok(Value::Decimal {
                        unscaled: dec.unscaled,
                        scale: dec.scale,
                    })
                }
                NumericDefault::Null => Ok(Value::Null),
            },
            _ => Err(Error::Unsupported(format!(
                "DefaultValue for {} column '{}' is not supported",
                data_type.get_name(),
                col.name
            ))),
        }
    }

    fn describe(&self) -> String {
        match self {
            NumericDefault::Null => "Null".to_string(),
            NumericDefault::Int(i) => i.to_string(),
            NumericDefault::Number { text, .. } => text.clone(),
        }
    }

    /// `getAsLongInt`: round half even to an `int` (`intValueExact`).
    fn as_long_int(&self) -> Result<i32> {
        match self {
            NumericDefault::Null => Ok(0),
            NumericDefault::Int(i) => Ok(*i),
            NumericDefault::Number { dec, .. } => {
                let value = dec.round_to_i128()?;
                i32::try_from(value).map_err(|_| {
                    Error::Jackcess(format!("DefaultValue {} is out of range", dec.display()))
                })
            }
        }
    }

    /// `getAsBigDecimal` followed by `longValue()` (which truncates)
    fn as_big_int(&self) -> Result<i64> {
        match self {
            NumericDefault::Null => Ok(0),
            NumericDefault::Int(i) => Ok(*i as i64),
            NumericDefault::Number { dec, .. } => {
                let value = dec.trunc_to_i128()?;
                i64::try_from(value).map_err(|_| {
                    Error::Jackcess(format!("DefaultValue {} is out of range", dec.display()))
                })
            }
        }
    }

    fn as_f64(&self) -> f64 {
        match self {
            NumericDefault::Null => 0.0,
            NumericDefault::Int(i) => *i as f64,
            NumericDefault::Number { text, dec } => {
                text.parse::<f64>().unwrap_or_else(|_| dec.to_f64())
            }
        }
    }
}

/// Parses a DEFAULT_VALUE numeric literal / constant.  The grammar is the one
/// `ExpressionTokenizer.maybeParseNumberLiteral` accepts (an optional sign
/// followed by digits, an optional fraction and an optional exponent) plus the
/// constant words the expression parser recognises.
fn parse_numeric_default(expr: &str, col_name: &str) -> Result<NumericDefault> {
    let lower = expr.to_ascii_lowercase();
    match lower.as_str() {
        "null" => return Ok(NumericDefault::Null),
        "true" | "yes" | "on" => return Ok(NumericDefault::Int(-1)),
        "false" | "no" | "off" => return Ok(NumericDefault::Int(0)),
        _ => {}
    }

    let (text, quoted) = if expr.starts_with('"') || expr.starts_with('\'') {
        (parse_quoted_default(expr, col_name)?, true)
    } else {
        (expr.to_string(), false)
    };

    if let Some(dec) = DecimalLiteral::parse(&text) {
        // a plain integer which fits in an `int` tokenizes as a LongValue
        if !quoted {
            if let Ok(i) = text.parse::<i32>() {
                // `+7`/`007` are integer tokens too
                if dec.scale == 0 && dec.unscaled == i as i128 {
                    return Ok(NumericDefault::Int(i));
                }
            }
        }
        return Ok(NumericDefault::Number { text, dec });
    }

    if quoted {
        return Err(Error::Jackcess(format!(
            "Invalid DefaultValue for column '{}': {} is not a number",
            col_name, text
        )));
    }
    Err(Error::Jackcess(format!(
        "Invalid DefaultValue for column '{}': {}",
        col_name, expr
    )))
}

/// A `BigDecimal`-like decimal: `unscaled * 10^-scale`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DecimalLiteral {
    unscaled: i128,
    scale: i32,
}

impl DecimalLiteral {
    /// Parses `[+-]digits[.digits][(e|E)[+-]digits]` exactly (the tokenizer's
    /// numeric literal grammar).
    fn parse(text: &str) -> Option<DecimalLiteral> {
        let bytes = text.as_bytes();
        if bytes.is_empty() {
            return None;
        }
        let mut i = 0usize;
        let negative = match bytes[0] {
            b'-' => {
                i = 1;
                true
            }
            b'+' => {
                i = 1;
                false
            }
            _ => false,
        };
        let mut unscaled: i128 = 0;
        let mut digits = 0usize;
        let mut int_digits = 0usize;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            unscaled = unscaled
                .checked_mul(10)?
                .checked_add((bytes[i] - b'0') as i128)?;
            digits += 1;
            int_digits += 1;
            i += 1;
        }
        // the tokenizer only tries a numeric literal when the first character
        // is a digit, so `.5` is not a number literal
        if int_digits == 0 {
            return None;
        }
        let mut scale: i32 = 0;
        if i < bytes.len() && bytes[i] == b'.' {
            i += 1;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                unscaled = unscaled
                    .checked_mul(10)?
                    .checked_add((bytes[i] - b'0') as i128)?;
                scale = scale.checked_add(1)?;
                digits += 1;
                i += 1;
            }
        }
        if digits == 0 {
            return None;
        }
        if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
            i += 1;
            let mut exp_negative = false;
            if i < bytes.len() && (bytes[i] == b'-' || bytes[i] == b'+') {
                exp_negative = bytes[i] == b'-';
                i += 1;
            }
            let mut exp: i32 = 0;
            let mut exp_digits = 0usize;
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                exp = exp.checked_mul(10)?.checked_add((bytes[i] - b'0') as i32)?;
                exp_digits += 1;
                i += 1;
            }
            if exp_digits == 0 {
                return None;
            }
            scale = if exp_negative {
                scale.checked_add(exp)?
            } else {
                scale.checked_sub(exp)?
            };
        }
        if i != bytes.len() {
            return None;
        }
        if negative {
            unscaled = -unscaled;
        }
        Some(DecimalLiteral { unscaled, scale })
    }

    fn pow10(exp: i32) -> Option<i128> {
        if !(0..=38).contains(&exp) {
            return None;
        }
        let mut value: i128 = 1;
        for _ in 0..exp {
            value = value.checked_mul(10)?;
        }
        Some(value)
    }

    /// Rounds to an integer with `RoundingMode.HALF_EVEN`
    /// (`BigDecimal.setScale(0, HALF_EVEN)`).
    fn round_to_i128(&self) -> Result<i128> {
        if self.scale <= 0 {
            let factor = Self::pow10(-self.scale).ok_or_else(|| self.out_of_range())?;
            return self
                .unscaled
                .checked_mul(factor)
                .ok_or_else(|| self.out_of_range());
        }
        let factor = Self::pow10(self.scale).ok_or_else(|| self.out_of_range())?;
        let quotient = self.unscaled / factor;
        let remainder = self.unscaled % factor;
        let half = factor / 2;
        let abs_remainder = remainder.abs();
        let round_up = abs_remainder > half || (abs_remainder == half && quotient % 2 != 0);
        if round_up {
            Ok(quotient + self.unscaled.signum())
        } else {
            Ok(quotient)
        }
    }

    /// `BigDecimal.longValue()`'s truncation towards zero.
    fn trunc_to_i128(&self) -> Result<i128> {
        if self.scale <= 0 {
            let factor = Self::pow10(-self.scale).ok_or_else(|| self.out_of_range())?;
            return self
                .unscaled
                .checked_mul(factor)
                .ok_or_else(|| self.out_of_range());
        }
        let factor = Self::pow10(self.scale).ok_or_else(|| self.out_of_range())?;
        Ok(self.unscaled / factor)
    }

    fn to_f64(self) -> f64 {
        let value = self.unscaled as f64;
        if self.scale >= 0 {
            match Self::pow10(self.scale) {
                Some(factor) => value / factor as f64,
                None => value / f64::INFINITY,
            }
        } else {
            match Self::pow10(-self.scale) {
                Some(factor) => value * factor as f64,
                None => value * f64::INFINITY,
            }
        }
    }

    fn display(&self) -> String {
        format!("{}e-{}", self.unscaled, self.scale)
    }

    fn out_of_range(&self) -> Error {
        Error::Jackcess(format!("DefaultValue {} is out of range", self.display()))
    }
}

/// `true` when `dec` can be written at the given scale without rounding
/// (`BigDecimal.setScale(target)`, which raises an `ArithmeticException`
/// instead of rounding).
fn fits_scale(dec: &DecimalLiteral, target: i32) -> bool {
    if dec.scale <= target {
        return true;
    }
    match dec.scale.checked_sub(target) {
        Some(diff) if diff > 0 => match DecimalLiteral::pow10(diff) {
            Some(factor) => dec.unscaled % factor == 0,
            None => false,
        },
        Some(_) => true,
        None => false,
    }
}

/// The number of decimal places the exact binary value of `value` needs (the
/// negated exponent of its lowest set bit), or `None` for NaN/infinity.
/// `BigDecimal(double)` expands the exact binary value, so `1.5` needs one
/// decimal place while `0.1` needs 55.
fn exact_decimal_places(value: f64) -> Option<i32> {
    if !value.is_finite() {
        return None;
    }
    let bits = value.abs().to_bits();
    if bits == 0 {
        return Some(0);
    }
    let exp_bits = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mantissa, exponent) = if exp_bits == 0 {
        (frac, -1074i32)
    } else {
        (frac | (1u64 << 52), exp_bits - 1075)
    };
    let low_bit_exponent = exponent + mantissa.trailing_zeros() as i32;
    Some((-low_bit_exponent).max(0))
}

/// The date/time conversion context of a database: its configured
/// [`DateTimeType`] and its time zone (`DatabaseImpl.getDateTimeFactory()` +
/// `getZoneId()`/`getTimeZone()`).
///
/// Java's `ColumnImpl` asks its database for both on every date conversion
/// (`ColumnImpl.getDateTimeFactory`, `getTimeZone`, `getZoneId`), which is why
/// every date conversion of the port takes this context explicitly.  The
/// default is the jackcess default: `LOCAL_DATE_TIME` plus the process default
/// zone ([`TimeZone::default`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateContext {
    /// the database's `Database.DateTimeType`
    pub date_time_type: DateTimeType,
    /// the database's time zone (`Database.getTimeZone`)
    pub time_zone: TimeZone,
}

impl DateContext {
    /// A context for the given database settings.
    pub fn new(date_time_type: DateTimeType, time_zone: TimeZone) -> DateContext {
        DateContext {
            date_time_type,
            time_zone,
        }
    }

    /// The jackcess defaults (`DEFAULT_DATE_TIME_TYPE` and the default zone).
    pub fn default_jackcess() -> DateContext {
        DateContext::new(DateTimeType::LocalDateTime, TimeZone::default())
    }
}

impl Default for DateContext {
    fn default() -> DateContext {
        DateContext::default_jackcess()
    }
}

/// [`date_value_bits_with`] for a database's [`DateContext`].
pub fn date_value_bits_in(value: &Value, context: DateContext) -> Result<u64> {
    date_value_bits_with(value, &context.time_zone, context.date_time_type)
}

/// [`date_value_from_bits`] for a database's [`DateContext`].
pub fn date_value_from_bits_in(bits: u64, context: DateContext) -> Value {
    date_value_from_bits(bits, context.date_time_type)
}

/// The stored bits of a date value, for a database whose `DateTimeType` is
/// `LOCAL_DATE_TIME`.
///
/// A [`Value::DateTime`] is already a local ("wall clock") date/time, so it is
/// written with the pure local conversion (`ColumnImpl.toDateDouble`).  A
/// numeric value is a `java.util.Date`-style epoch instant and is first
/// converted to the database zone's wall clock time
/// (`LocalDateTime.ofInstant(..., getZoneId())`), which is what the default
/// `LDTDateTimeFactory` does.
///
/// This is the conversion a caller which has no database context uses (the
/// default `LOCAL_DATE_TIME` factory); the row writer, the index entry encoder
/// and the property column all call [`date_value_bits_in`] /
/// [`date_value_bits_with`] with their database's [`DateContext`].
pub fn date_value_bits(value: &Value, tz: &TimeZone) -> Result<u64> {
    date_value_bits_with(value, tz, DateTimeType::LocalDateTime)
}

/// The stored bits of a date value according to the database's `DateTimeType`
/// (`ColumnImpl.writeDateValue` with the database's `DateTimeFactory`).
///
/// * a [`Value::DateBits`] is written with its original bits in both modes: it
///   is the port's `ColumnImpl.DateExt`, and Java's `writeDateValue` keeps the
///   bits of a value previously read from the database so a round trip loses no
///   precision;
/// * under `LOCAL_DATE_TIME` a [`Value::DateTime`] keeps its full nanosecond
///   precision and a numeric value is an epoch instant converted to the zone's
///   wall clock time (`LDTDateTimeFactory.toDateDouble`);
/// * under `DATE` a [`Value::DateTime`] goes through
///   `temporalToLocalDateTime(...).atZone(zone).toInstant()`, which truncates
///   the sub-millisecond part, and a numeric value is a `java.util.Date` epoch
///   instant (`DefaultDateTimeFactory.toDateDouble`).  Both are the same OLE
///   double as the `LOCAL_DATE_TIME` conversion for millisecond precision
///   values, which is why the mode only changes the written bytes of a
///   sub-millisecond date/time.
pub fn date_value_bits_with(
    value: &Value,
    tz: &TimeZone,
    date_time_type: DateTimeType,
) -> Result<u64> {
    match value {
        // `writeDateValue`: `DateExt.getDateBits()`, the exact stored bits
        Value::DateBits(bits) => Ok(bits.to_bits()),
        Value::Null => Ok(0f64.to_bits()),
        Value::DateTime(ldt) => Ok(match date_time_type {
            DateTimeType::LocalDateTime => datenums::to_date_double(*ldt),
            // `DefaultDateTimeFactory.toDateDouble`: the wall clock time is
            // converted to an instant in the database zone
            // (`LocalDateTime.atZone(zoneId).toInstant()`) and then shifted by
            // the `TimeZone` offset of that instant.  A fixed offset zone makes
            // the two offsets cancel (the plain wall clock double); a named
            // zone does not, around a DST transition or before 1900.
            DateTimeType::Date => datenums::to_date_double_from_local_datetime(*ldt, tz),
        }
        .to_bits()),
        v => {
            let millis = v.as_i64().unwrap_or(0);
            let double = match date_time_type {
                DateTimeType::LocalDateTime => {
                    datenums::to_date_double(tz.local_date_time_from_millis(millis))
                }
                DateTimeType::Date => datenums::to_date_double_from_millis(millis, tz),
            };
            Ok(double.to_bits())
        }
    }
}

/// Decode the stored bits of a `SHORT_DATE_TIME` value according to the
/// database's `DateTimeType` (`ColumnImpl.readDateValue`, i.e.
/// `DateTimeFactory.fromDateBits`).
///
/// * `LOCAL_DATE_TIME`: `ColumnImpl.ldtFromLocalDateDouble`, a
///   [`Value::DateTime`];
/// * `DATE`: `new DateExt(col.fromDateDouble(bits), bits)`, a `java.util.Date`
///   which also keeps the original bits.  The port has no separate variant for
///   it: [`Value::DateBits`] carries exactly those bits, and
///   [`date_ext_millis`] converts them to the `java.util.Date.getTime()` the
///   Java value reports.
pub fn date_value_from_bits(bits: u64, date_time_type: DateTimeType) -> Value {
    match date_time_type {
        DateTimeType::LocalDateTime => {
            Value::DateTime(ldt_from_local_date_double(f64::from_bits(bits)))
        }
        DateTimeType::Date => Value::DateBits(f64::from_bits(bits)),
    }
}

/// `ColumnImpl.DateExt.getTime()`: the epoch milliseconds a `DATE` mode date
/// value reports (`ColumnImpl.fromDateDouble` with the database zone).
pub fn date_ext_millis(bits: f64, tz: &TimeZone) -> i64 {
    datenums::from_date_double(bits, tz)
}

/// `ColumnImpl.toLocalDateTime(Object value, DatabaseImpl db)`: the
/// `LocalDateTime` Java returns for a value read with `DateTimeType.DATE` (for
/// instance `DatabaseImpl.getDateForObject` for the catalog created/updated
/// dates, which converts the `DateExt` with the database zone).
///
/// Java takes the `DateExt.getTime()` milliseconds (rounded to a whole
/// millisecond) and converts that instant to the database `ZoneId`
/// (`LocalDateTime.ofInstant(Instant.ofEpochMilli(millis), zoneId)`), so the
/// zone is consulted twice: once by `fromDateDouble` (the `TimeZone` face, raw
/// offset before 1900) and once by the instant conversion (the `ZoneId` face,
/// historical LMT included).
pub fn date_ext_local_date_time(bits: f64, tz: &TimeZone) -> LocalDateTime {
    let millis = datenums::from_date_double(bits, tz);
    tz.local_date_time_from_millis(millis)
}

/// The `java.util.Date.getTime()` of a row value read with
/// `DateTimeType.DATE`, or `None` for a value which is not a date
/// ([`Value::DateTime`] is the `LOCAL_DATE_TIME` representation, which has no
/// `Date` form).
pub fn date_value_millis(value: &Value, tz: &TimeZone) -> Option<i64> {
    match value {
        Value::DateBits(bits) => Some(date_ext_millis(*bits, tz)),
        _ => None,
    }
}

/// `ColumnImpl.ldtFromLocalDateDouble`
pub fn ldt_from_local_date_double(value: f64) -> LocalDateTime {
    let (secs, mut nanos) = datenums::duration_from_local_date_double(value);
    // `Duration` normalizes a nanosecond count of a whole second or more into
    // the seconds field (Java's Duration.ofSeconds does the same)
    let mut secs = secs;
    if nanos >= 1_000_000_000 {
        secs += nanos / 1_000_000_000;
        nanos %= 1_000_000_000;
    }
    let base = LocalDateTime::of_date(1899, 12, 30);
    let base_secs = base.to_epoch_day() * 86400;
    let total = base_secs + secs;
    let days = total.div_euclid(86400);
    let sec = total.rem_euclid(86400);
    let mut ldt = LocalDateTime::from_epoch_day(days);
    ldt.hour = (sec / 3600) as u32;
    ldt.minute = ((sec % 3600) / 60) as u32;
    ldt.second = (sec % 60) as u32;
    ldt.nano = nanos as u32;
    ldt
}

fn fix_numeric_byte_order(bytes: &mut [u8]) {
    // fix endianness of each 4 byte segment (ColumnImpl.fixNumericByteOrder)
    let mut i = 0;
    while i + 4 <= bytes.len() {
        bytes[i..i + 4].reverse();
        i += 4;
    }
}

fn bytes_to_unscaled(bytes: &[u8], negate: bool) -> i128 {
    let mut v: i128 = 0;
    for b in bytes {
        v = (v << 8) | (*b as i128);
    }
    if negate {
        -v
    } else {
        v
    }
}

fn unscaled_to_bytes(v: i128, max_len: usize) -> Vec<u8> {
    let mut v = v;
    let mut out = vec![0u8; max_len];
    for i in (0..max_len).rev() {
        out[i] = (v & 0xFF) as u8;
        v >>= 8;
    }
    out
}

fn to_mul4(val: i32) -> i32 {
    (val / 4) * 4
}

fn decimal_digits(value: i128) -> i32 {
    let mut v = value.unsigned_abs();
    if v == 0 {
        return 1;
    }
    let mut digits = 0;
    while v > 0 {
        v /= 10;
        digits += 1;
    }
    digits
}

/// `ColumnImpl.fixNumericByteOrder`: the bytes of a calculated numeric value.
/// Accesses truncates leading zero bytes and _then_ swaps the endianness,
/// which swaps the first 4 bytes separately (when the length is not a multiple
/// of 8) and then each 8 byte segment.
fn fix_calc_numeric_byte_order(bytes: &mut [u8]) {
    let mut pos = 0usize;
    if !bytes.len().is_multiple_of(8) {
        if bytes.len() >= 4 {
            bytes[0..4].reverse();
        }
        pos = 4;
    }
    while pos + 8 <= bytes.len() {
        bytes[pos..pos + 8].reverse();
        pos += 8;
    }
}

/// `ColumnImpl.toUnscaledByteArray`: the big endian bytes of the (positive)
/// unscaled value, left padded to `max_len` bytes.
fn unscaled_to_bytes_checked(value: i128, max_len: usize) -> Result<Vec<u8>> {
    let mut mag = value.unsigned_abs();
    let mut len = 0usize;
    let mut tmp = mag;
    while tmp > 0 {
        len += 1;
        tmp >>= 8;
    }
    if len > max_len {
        return Err(Error::InvalidValue(
            "Too many bytes for valid BigInteger?".into(),
        ));
    }
    let mut out = vec![0u8; max_len];
    for i in (max_len - len..max_len).rev() {
        out[i] = (mag & 0xFF) as u8;
        mag >>= 8;
    }
    Ok(out)
}

/// `ColumnImpl.toBigDecimal(Object)` for the value types a calculated numeric
/// column can receive from the expression engine.
fn value_as_decimal_parts(value: &Value) -> Result<(i128, i32)> {
    let parts = match value {
        Value::Decimal { unscaled, scale } => (*unscaled, *scale),
        Value::Byte(v) => (*v as i128, 0),
        Value::Int(v) => (*v as i128, 0),
        Value::Long(v) => (*v as i128, 0),
        Value::BigInt(v) => (*v as i128, 0),
        Value::Bool(b) => (if *b { -1 } else { 0 }, 0),
        Value::Null => (0, 0),
        Value::Float(f) => {
            let dec = crate::expr::Decimal::from_f64(*f as f64)
                .map_err(|e| Error::Jackcess(e.to_string()))?;
            (dec.unscaled(), dec.scale())
        }
        Value::Double(d) => {
            let dec =
                crate::expr::Decimal::from_f64(*d).map_err(|e| Error::Jackcess(e.to_string()))?;
            (dec.unscaled(), dec.scale())
        }
        _ => {
            return Err(Error::Jackcess(format!(
                "cannot convert value to a calculated numeric value: {:?}",
                value
            )))
        }
    };
    Ok(parts)
}

fn read_guid_bytes(buf: &mut ByteBuf) -> Result<[u8; 16]> {
    let bytes = buf.get_bytes(16)?;
    let mut out = [0u8; 16];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn write_guid_value(buffer: &mut ByteBuf, value: &Value) -> Result<()> {
    match value {
        Value::Uuid(b) => buffer.put_bytes(b),
        Value::Text(s) => {
            let mut b = parse_guid(s)?;
            if buffer.order() != ByteOrder::BigEndian {
                // the first three GUID components are integers and respect
                // endianness (ColumnImpl.writeGUIDValue)
                b[0..4].reverse();
                b[4..6].reverse();
                b[6..8].reverse();
            }
            buffer.put_bytes(&b)
        }
        _ => Err(Error::Jackcess("invalid GUID value".into())),
    }
}

pub fn parse_guid(s: &str) -> Result<[u8; 16]> {
    let t: String = s
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .chars()
        .filter(|c| *c != '-')
        .collect();
    if t.len() != 32 {
        return Err(Error::Jackcess(format!("invalid GUID {s}")));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&t[i * 2..i * 2 + 2], 16)
            .map_err(|_| Error::Jackcess(format!("invalid GUID {s}")))?;
    }
    Ok(out)
}

/// `ColumnImpl.GUID_PATTERN`: whether the given value is a textual GUID the way
/// jackcess accepts one (`[ \t]*[{]?8-4-4-4-12[}]?[ \t]*`, hex digits only).
/// Unlike [`parse_guid`], the hyphens must be in the canonical positions and
/// the braces may only surround the whole value.
pub fn is_guid_string(s: &str) -> bool {
    let t = s.trim();
    let t = t.strip_prefix('{').unwrap_or(t);
    let t = t.strip_suffix('}').unwrap_or(t);
    let mut parts = t.split('-');
    let mut expected = [8usize, 4, 4, 4, 12].into_iter();
    loop {
        match (parts.next(), expected.next()) {
            (Some(part), Some(len)) => {
                if part.len() != len || !part.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return false;
                }
            }
            (None, None) => return true,
            _ => return false,
        }
    }
}

/// `ColumnImpl.toGUIDString(UUID.randomUUID())`: a fresh random version 4
/// (RFC 4122 variant) UUID in the braced upper case form jackcess stores and
/// reads back for a GUID column.
pub fn random_guid_string() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::Jackcess(format!("random GUID generation failed: {e}")))?;
    // `UUID.randomUUID` (constructor): set the version (4) and variant (2) bits
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    Ok(format!(
        "{{{}-{}-{}-{}-{}}}",
        hex_upper(&bytes[0..4]),
        hex_upper(&bytes[4..6]),
        hex_upper(&bytes[6..8]),
        hex_upper(&bytes[8..10]),
        hex_upper(&bytes[10..16])
    ))
}

fn hex_upper(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(DIGITS[(b >> 4) as usize] as char);
        out.push(DIGITS[(b & 0x0F) as usize] as char);
    }
    out
}

/// The 16 bytes `ColumnImpl.writeGUIDValue` puts on the page for the given
/// canonical (`java.util.UUID.toString`) bytes.  The first three components are
/// integer fields, so they are stored in the page's byte order.
pub fn guid_stored_bytes(canonical: &[u8; 16], order: ByteOrder) -> [u8; 16] {
    let mut out = *canonical;
    if order != ByteOrder::BigEndian {
        out[0..4].reverse();
        out[4..6].reverse();
        out[6..8].reverse();
    }
    out
}

/// The inverse of [`guid_stored_bytes`]: the canonical byte order of a GUID
/// value read back from a page.
pub fn guid_canonical_bytes(stored: &[u8; 16], order: ByteOrder) -> [u8; 16] {
    // the transformation is its own inverse
    guid_stored_bytes(stored, order)
}

pub const LONG_VALUE_TYPE_THIS_PAGE: u8 = 0x80;
pub const LONG_VALUE_TYPE_OTHER_PAGE: u8 = 0x40;
pub const LONG_VALUE_TYPE_OTHER_PAGES: u8 = 0x00;
const LONG_VALUE_TYPE_MASK: u32 = 0xC000_0000;

/// `LongValueColumnImpl.readLongValue` - reads the bytes of a long value.
pub fn read_long_value(
    ch: &mut PageChannel,
    lval_definition: &[u8],
    format: &JetFormat,
) -> Result<Vec<u8>> {
    let mut def = ByteBuf::wrap(lval_definition.to_vec(), ByteOrder::LittleEndian);
    let length_with_flags = def.get_u32()?;
    let length = (length_with_flags & !LONG_VALUE_TYPE_MASK) as usize;
    let value_type = ((length_with_flags & LONG_VALUE_TYPE_MASK) >> 24) as u8;
    let mut rtn = vec![0u8; length];

    if value_type == LONG_VALUE_TYPE_THIS_PAGE {
        let _ = def.get_u32()?;
        let _ = def.get_u32()?;
        let row_len = def.remaining();
        let n = row_len.min(length);
        rtn.truncate(n);
        def.read_into(&mut rtn)?;
    } else {
        if lval_definition.len() != format.size_long_value_def {
            return Err(Error::Jackcess(format!(
                "Expected {} bytes in long value definition, but found {}",
                format.size_long_value_def,
                lval_definition.len()
            )));
        }
        let row_num = def.get_u8()? as usize;
        let page_num = crate::byteutil::get3_byte_int(&mut def)?;
        let mut lval_page = ch.create_page_buffer();
        match value_type {
            LONG_VALUE_TYPE_OTHER_PAGE => {
                ch.read_page(&mut lval_page, page_num)?;
                let row_start = crate::table::find_row_start(&lval_page, row_num, format)? as usize;
                let row_end = crate::table::find_row_end(&lval_page, row_num, format)?;
                let row_len = row_end.saturating_sub(row_start);
                let n = row_len.min(length);
                rtn.truncate(n);
                let data = lval_page.get_bytes_at(row_start, n)?;
                rtn.copy_from_slice(&data);
            }
            LONG_VALUE_TYPE_OTHER_PAGES => {
                let mut out: Vec<u8> = Vec::with_capacity(length);
                let mut remaining_len = length;
                let mut cur_page = page_num;
                let mut cur_row = row_num;
                while remaining_len > 0 {
                    lval_page.clear();
                    ch.read_page(&mut lval_page, cur_page)?;
                    let row_start =
                        crate::table::find_row_start(&lval_page, cur_row, format)? as usize;
                    let row_end = crate::table::find_row_end(&lval_page, cur_row, format)?;
                    cur_row = lval_page.get_u8_at(row_start)? as usize;
                    cur_page = crate::byteutil::get3_byte_int_at(
                        &lval_page,
                        row_start + 1,
                        ByteOrder::LittleEndian,
                    )?;
                    let mut chunk_len = (row_end - row_start).saturating_sub(4);
                    let take = chunk_len.min(remaining_len);
                    let data = lval_page.get_bytes_at(row_start + 4, take)?;
                    out.extend_from_slice(&data);
                    remaining_len -= take;
                    chunk_len = take;
                    let _ = chunk_len;
                }
                rtn = out;
            }
            _ => {
                return Err(Error::Jackcess(format!(
                    "Unrecognized long value type: {}",
                    value_type
                )))
            }
        }
    }
    Ok(rtn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatype::DataType;

    /// A bare column definition for the default value / validator tests.
    pub(super) fn column(name: &str, data_type: DataType, default: Option<&str>) -> Column {
        Column {
            name: name.to_string(),
            data_type,
            column_number: 1,
            column_id: 1,
            column_length: 20,
            variable_length: data_type.is_variable_length(),
            auto_number: false,
            calculated: false,
            complex_value_fk: false,
            hidden: false,
            security_identifier: false,
            var_len_table_index: 0,
            fixed_data_offset: 0,
            flags: 0,
            ext_flags: 0,
            precision: 0,
            scale: 0,
            sort_order: None,
            code_page: 0,
            compressed_unicode: false,
            display_index: 1,
            column_index: 1,
            umap_page_number: -1,
            umap_owned_row_number: 0,
            umap_free_row_number: 0,
            append_only: false,
            complex_id: 0,
            length_in_units: -1,
            default_value: default.map(|s| s.to_string()),
            required: false,
            allow_zero_length: true,
            column_validator: None,
        }
    }

    fn default_of(data_type: DataType, expr: &str) -> Result<Value> {
        column("C", data_type, Some(expr)).generate_default_value()
    }

    #[test]
    fn default_value_literals_match_java() {
        // the values the Java expression wrapper (`evalValue` with the
        // column's `Value.Type`) produces and `TableImpl` writes
        assert_eq!(default_of(DataType::Long, "7").unwrap(), Value::Long(7));
        // `getAsLongInt` rounds half even (BigDecimal 1.5 -> 2)
        assert_eq!(default_of(DataType::Long, "1.5").unwrap(), Value::Long(2));
        assert_eq!(default_of(DataType::Long, "2.5").unwrap(), Value::Long(2));
        assert_eq!(default_of(DataType::Long, "3.5").unwrap(), Value::Long(4));
        assert_eq!(default_of(DataType::Long, "-1").unwrap(), Value::Long(-1));
        assert_eq!(default_of(DataType::Long, "+7").unwrap(), Value::Long(7));
        assert_eq!(default_of(DataType::Long, "007").unwrap(), Value::Long(7));
        assert_eq!(default_of(DataType::Long, "TRUE").unwrap(), Value::Long(-1));
        assert_eq!(default_of(DataType::Long, "false").unwrap(), Value::Long(0));
        assert_eq!(default_of(DataType::Long, "NULL").unwrap(), Value::Null);
        assert_eq!(default_of(DataType::Int, "65537").unwrap(), Value::Int(1));
        assert_eq!(default_of(DataType::Byte, "300").unwrap(), Value::Byte(44));
        assert_eq!(
            default_of(DataType::Double, "1.5").unwrap(),
            Value::Double(1.5)
        );
        assert_eq!(
            default_of(DataType::Double, "1e2").unwrap(),
            Value::Double(100.0)
        );
        assert_eq!(
            default_of(DataType::Float, "1.5").unwrap(),
            Value::Float(1.5)
        );
        // `writeNumericValue` calls `setScale(column scale)`; a literal with
        // the same number of decimal places is written exactly
        let mut numeric = column("N", DataType::Numeric, Some("1.5"));
        numeric.scale = 1;
        assert_eq!(
            numeric.generate_default_value().unwrap(),
            Value::Decimal {
                unscaled: 15,
                scale: 1
            }
        );
        // ... and one which would need rounding is rejected like jackcess
        assert!(matches!(
            default_of(DataType::Numeric, "1.5"),
            Err(Error::Unsupported(_))
        ));
        // `writeCurrencyValue` stores `BigDecimal(double).setScale(4)`;
        // exactly representable values work, 0.1 is rejected like jackcess
        assert_eq!(
            default_of(DataType::Money, "1.5").unwrap(),
            Value::Double(1.5)
        );
        assert!(matches!(
            default_of(DataType::Money, "0.1"),
            Err(Error::Unsupported(_))
        ));
        // `BigDecimal.longValue` truncates towards zero
        assert_eq!(
            default_of(DataType::BigInt, "1.9").unwrap(),
            Value::BigInt(1)
        );
        assert_eq!(
            default_of(DataType::Boolean, "TRUE").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            default_of(DataType::Boolean, "-1").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            default_of(DataType::Boolean, "0").unwrap(),
            Value::Bool(false)
        );

        // textual columns take the whole trimmed property as the literal text
        assert_eq!(
            default_of(DataType::Text, "\"abc\"").unwrap(),
            Value::Text("abc".to_string())
        );
        assert_eq!(
            default_of(DataType::Text, "\"a\"\"b\"").unwrap(),
            Value::Text("a\"b".to_string())
        );
        assert_eq!(
            default_of(DataType::Text, "'abc'").unwrap(),
            Value::Text("abc".to_string())
        );
        assert_eq!(
            default_of(DataType::Text, "unquoted text").unwrap(),
            Value::Text("unquoted text".to_string())
        );
        assert_eq!(
            default_of(DataType::Text, "123").unwrap(),
            Value::Text("123".to_string())
        );
        assert_eq!(
            default_of(DataType::Memo, "\"memo default\"").unwrap(),
            Value::Text("memo default".to_string())
        );

        // no property means no default
        assert_eq!(
            column("C", DataType::Long, None)
                .generate_default_value()
                .unwrap(),
            Value::Null
        );
    }

    #[test]
    fn unsupported_default_value_forms_are_reported() {
        // full expression mode (`Expressionator.isLiteralDefaultValue`)
        for (data_type, expr) in [
            (DataType::Long, "=1+1"),
            (DataType::Double, "=Now()"),
            (DataType::Text, "=UCase(\"a\")"),
            // date literals and object references run through the expression
            // engine instead of the literal text path
            (DataType::Text, "#2020-01-01#"),
            (DataType::Text, "[Name]"),
            // a date default accepts the ISO `#...#` literal and a numeric
            // literal, but not an Access expression or a locale date
            (DataType::ShortDateTime, "#1/2/2020#"),
            (DataType::ShortDateTime, "#abc#"),
            (DataType::ShortDateTime, "[Name]"),
            (DataType::ExtDateTime, "0"),
        ] {
            let err = default_of(data_type, expr).expect_err(expr);
            assert!(
                matches!(err, Error::Unsupported(_)),
                "{} should be unsupported, got {:?}",
                expr,
                err
            );
        }
    }

    /// A `SHORT_DATE_TIME` default value accepts the ISO `#...#` date literal
    /// (Java evaluates it with the expression engine and stores the
    /// `LocalDateTime`) and a numeric literal (a `java.util.Date` epoch value).
    #[test]
    fn date_default_values_are_parsed() {
        assert_eq!(
            default_of(DataType::ShortDateTime, "#2020-03-15 12:30:45#").unwrap(),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0))
        );
        assert_eq!(
            default_of(DataType::ShortDateTime, "#2020-03-15#").unwrap(),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 0, 0, 0, 0))
        );
        // a fractional second and the `T` separator
        assert_eq!(
            default_of(DataType::ShortDateTime, "#1999-12-31T23:59:59.123#").unwrap(),
            Value::DateTime(LocalDateTime::new(1999, 12, 31, 23, 59, 59, 123_000_000))
        );
        // a time only literal uses the OLE base date (`ColumnImpl.BASE_LD`)
        assert_eq!(
            default_of(DataType::ShortDateTime, "#12:30:45#").unwrap(),
            Value::DateTime(LocalDateTime::new(1899, 12, 30, 12, 30, 45, 0))
        );
        // a numeric literal is a `java.util.Date` instant, which the row writer
        // converts with the database zone
        assert_eq!(
            default_of(DataType::ShortDateTime, "1584246645000").unwrap(),
            Value::BigInt(1_584_246_645_000)
        );
        assert_eq!(
            default_of(DataType::ShortDateTime, "Null").unwrap(),
            Value::Null
        );
        // an EXT_DATE_TIME default only accepts the date literal
        assert_eq!(
            default_of(DataType::ExtDateTime, "#2020-03-15#").unwrap(),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 0, 0, 0, 0))
        );
        // an impossible date is rejected like the expression engine rejects it
        for expr in ["#2020-13-01#", "#2020-02-30#", "#2020-01-01 24:00:00#"] {
            let err = default_of(DataType::ShortDateTime, expr).expect_err(expr);
            assert!(
                matches!(err, Error::Unsupported(_)),
                "{} should be rejected, got {:?}",
                expr,
                err
            );
        }
    }

    #[test]
    fn invalid_default_value_literals_are_rejected() {
        for (data_type, expr) in [
            (DataType::Long, "abc"),
            (DataType::Double, ".5"),
            (DataType::Long, "1.2.3"),
            (DataType::Text, "\"unterminated"),
        ] {
            assert!(default_of(data_type, expr).is_err(), "{}", expr);
        }
    }

    #[test]
    fn internal_validators_match_java() {
        // `RequiredColValidator`
        let mut required = column("Req", DataType::Long, None);
        required.required = true;
        assert_eq!(
            required.validate_value(Value::Null),
            Err(ColumnValidation::Required)
        );
        assert_eq!(required.validate_value(Value::Long(1)), Ok(Value::Long(1)));

        // `NoZeroLenColValidator`: null is allowed, an empty string is not,
        // and a non-empty value is handed back as its text form
        let mut no_zero = column("Name", DataType::Text, None);
        no_zero.allow_zero_length = false;
        assert_eq!(no_zero.validate_value(Value::Null), Ok(Value::Null));
        assert_eq!(
            no_zero.validate_value(Value::Text(String::new())),
            Err(ColumnValidation::ZeroLength)
        );
        assert_eq!(
            no_zero.validate_value(Value::Text("x".to_string())),
            Ok(Value::Text("x".to_string()))
        );
        assert_eq!(
            no_zero.validate_value(Value::Long(5)),
            Ok(Value::Text("5".to_string()))
        );
        // the zero length validator only applies to textual columns
        let mut numeric = column("N", DataType::Long, None);
        numeric.allow_zero_length = false;
        assert_eq!(numeric.validate_value(Value::Long(0)), Ok(Value::Long(0)));

        // both validators: null is required, "" is zero length
        let mut both = column("B", DataType::Text, None);
        both.required = true;
        both.allow_zero_length = false;
        assert_eq!(
            both.validate_value(Value::Null),
            Err(ColumnValidation::Required)
        );
        assert_eq!(
            both.validate_value(Value::Text(String::new())),
            Err(ColumnValidation::ZeroLength)
        );
    }

    /// The `ValidationRule` / `ValidationText` properties: field rules get the
    /// column value as the implicit left value, row rules resolve identifiers
    /// against the row, `ValidationText` replaces the default message, and a
    /// malformed rule is kept as the error Java raises on evaluation.
    #[test]
    fn validation_rules_match_java_semantics() {
        use crate::expr::{EvalContext, ExprValue};

        // a field rule whose expression is only a comparison already is a
        // validation expression: `[ThisColumn] > 3`
        let rule = ValidationRule::field("> 3", None);
        assert!(rule.text.is_none());
        let expr = rule.expression.as_ref().expect("parsed field rule");
        let mut context = EvalContext::new();
        context.set_table_name("Test");
        context.set_column("Num", ExprValue::Long(5));
        context.set_this_value(ExprValue::Long(5));
        assert_eq!(expr.eval_condition(&context), Ok(true));
        context.set_this_value(ExprValue::Long(3));
        assert_eq!(expr.eval_condition(&context), Ok(false));

        // a non-validation expression is compared with the field value
        // (`EImplicitCompOp`): a field rule of `5` accepts the value 5
        let implicit = ValidationRule::field("5", None);
        let expr = implicit.expression.as_ref().expect("parsed implicit rule");
        context.set_this_value(ExprValue::Long(5));
        assert_eq!(expr.eval_condition(&context), Ok(true));
        context.set_this_value(ExprValue::Long(4));
        assert_eq!(expr.eval_condition(&context), Ok(false));

        // a function based field rule, `Len([Code]) = 3`
        let len_rule = ValidationRule::field("Len([Code]) = 3", None);
        let expr = len_rule.expression.as_ref().expect("parsed Len rule");
        let mut code_context = EvalContext::new();
        code_context.set_table_name("Test");
        code_context.set_column("Code", ExprValue::Text("abc".into()));
        code_context.set_this_value(ExprValue::Text("abc".into()));
        assert_eq!(expr.eval_condition(&code_context), Ok(true));
        code_context.set_column("Code", ExprValue::Text("ab".into()));
        code_context.set_this_value(ExprValue::Text("ab".into()));
        assert_eq!(expr.eval_condition(&code_context), Ok(false));

        // a row rule can reference two columns
        let row_rule = ValidationRule::row("[Start] <= [End]", Some("Start must not be after End"));
        let expr = row_rule.expression.as_ref().expect("parsed row rule");
        let mut row_context = EvalContext::new();
        row_context.set_table_name("Test");
        row_context
            .set_column("Start", ExprValue::Long(5))
            .set_column("End", ExprValue::Long(6));
        assert_eq!(expr.eval_condition(&row_context), Ok(true));
        row_context.set_column("End", ExprValue::Long(3));
        assert_eq!(expr.eval_condition(&row_context), Ok(false));

        // `ValidationText` wins over the default message
        assert_eq!(
            row_rule.message(|| "Invalid row".to_string()),
            "Start must not be after End"
        );
        assert_eq!(
            len_rule.message(|| "Invalid column value 'ab'".to_string()),
            "Invalid column value 'ab'"
        );

        // the default column message text is Java's string concatenation of
        // the rejected value
        assert_eq!(value_message_text(&Value::Long(-5)), "-5");
        assert_eq!(value_message_text(&Value::Text("ab".to_string())), "ab");
        assert_eq!(value_message_text(&Value::Null), "null");
        assert_eq!(value_message_text(&Value::Bool(true)), "true");
        assert_eq!(value_message_text(&Value::Double(1.5)), "1.5");
        assert_eq!(value_message_text(&Value::Double(2.0)), "2.0");

        // `ValidationRule` is trimmed when the property is read; the rule
        // constructor itself keeps what it is given (the callers trim)
        assert_eq!(
            ValidationRule::row("  [Start] <= [End]  ", Some(" x "))
                .text
                .as_deref(),
            Some(" x ")
        );

        // a malformed rule is kept as the error reported on evaluation
        let bad = ValidationRule::field("[Num] >", None);
        assert!(bad.expression.is_err(), "a malformed rule must be rejected");
    }
}

#[cfg(test)]
mod regression_tests {
    use super::tests::column;
    use super::*;
    use crate::database::Database;
    use crate::datatype::DataType;

    /// `Value::Byte` holds the Java `Byte` bit pattern: 0xFF is -1, not 255.
    #[test]
    fn byte_column_writes_and_reads_signed_bytes() {
        let mut col = Column {
            name: "B".into(),
            data_type: DataType::Byte,
            column_number: 0,
            column_id: 0,
            column_length: 1,
            variable_length: false,
            auto_number: false,
            calculated: false,
            complex_value_fk: false,
            hidden: false,
            security_identifier: false,
            var_len_table_index: 0,
            fixed_data_offset: 0,
            flags: 0,
            ext_flags: 0,
            precision: 0,
            scale: 0,
            sort_order: None,
            code_page: 0,
            compressed_unicode: false,
            display_index: 0,
            column_index: 0,
            umap_page_number: -1,
            umap_owned_row_number: 0,
            umap_free_row_number: 0,
            append_only: false,
            complex_id: -1,
            length_in_units: -1,
            default_value: None,
            required: false,
            allow_zero_length: true,
            column_validator: None,
        };
        let format = JetFormat::version_4();
        let tz = TimeZone::default();
        for (value, expected) in [
            (Value::Byte(255), 0xFFu8),
            (Value::Byte(0), 0x00),
            (Value::Byte(127), 0x7F),
            (Value::Byte(128), 0x80),
        ] {
            let bytes = col
                .write_value(
                    &value,
                    crate::bytebuf::ByteOrder::LittleEndian,
                    Charset::Utf16Le,
                    &format,
                    &tz,
                    DateTimeType::LocalDateTime,
                )
                .expect("write byte");
            assert_eq!(bytes, vec![expected], "writing {value:?}");
            let read = col
                .read_value(
                    &bytes,
                    crate::bytebuf::ByteOrder::LittleEndian,
                    Charset::Utf16Le,
                    &format,
                    DateTimeType::LocalDateTime,
                )
                .expect("read byte");
            assert_eq!(read, Value::Byte(expected));
        }
        let _ = &mut col;
    }

    /// Java's `Duration.ofSeconds` carries a whole second of nanoseconds into
    /// the seconds field; the OLE date decoding must do the same, so
    /// `LocalDateTime.nano` never reaches 1_000_000_000.
    #[test]
    fn date_double_roundtrip_never_yields_a_full_second_of_nanos() {
        use crate::datenums::{to_date_double, LocalDateTime};
        for (h, m, s, n) in [
            (0u32, 0u32, 0u32, 0u32),
            (8, 9, 10, 0),
            (23, 59, 59, 999_000_000),
            (12, 30, 45, 123_000_000),
        ] {
            let ldt = LocalDateTime::new(2022, 6, 7, h, m, s, n);
            let d = to_date_double(ldt);
            let back = ldt_from_local_date_double(d);
            assert!(
                back.nano < 1_000_000_000,
                "nano out of range for {ldt:?}: {back:?}"
            );
            assert_eq!(
                (back.year, back.month, back.day, back.hour, back.minute),
                (2022, 6, 7, h, m),
                "date/time changed for {ldt:?}: {back:?}"
            );
            assert!(
                back.second == s || (back.second == s + 1 && back.nano == 0),
                "unexpected second for {ldt:?}: {back:?}"
            );
            assert!(
                (back.nano as i64 - n as i64).abs() <= 1_000_000,
                "unexpected nanos for {ldt:?}: {back:?}"
            );
        }
    }

    // -----------------------------------------------------------------
    // DateTimeType conversions (`ColumnImpl.DateTimeFactory`)
    // -----------------------------------------------------------------

    fn short_date_time_column() -> Column {
        Column {
            name: "D".into(),
            data_type: DataType::ShortDateTime,
            column_number: 0,
            column_id: 0,
            column_length: 8,
            variable_length: false,
            auto_number: false,
            calculated: false,
            complex_value_fk: false,
            hidden: false,
            security_identifier: false,
            var_len_table_index: 0,
            fixed_data_offset: 0,
            flags: 0,
            ext_flags: 0,
            precision: 0,
            scale: 0,
            sort_order: None,
            code_page: 0,
            compressed_unicode: false,
            display_index: 0,
            column_index: 0,
            umap_page_number: -1,
            umap_owned_row_number: 0,
            umap_free_row_number: 0,
            append_only: false,
            complex_id: 0,
            length_in_units: -1,
            default_value: None,
            required: false,
            allow_zero_length: true,
            column_validator: None,
        }
    }

    fn read_short_date_time(bits: u64, date_time_type: DateTimeType) -> Value {
        short_date_time_column()
            .read_value(
                &bits.to_le_bytes(),
                ByteOrder::LittleEndian,
                Charset::Utf16Le,
                &JetFormat::version_4(),
                date_time_type,
            )
            .unwrap()
    }

    /// `DateTimeType.DATE` reads a `SHORT_DATE_TIME` as the `DateExt` analogue:
    /// the stored bits are kept (`Value::DateBits`) and `Date.getTime()`
    /// (`date_value_millis`) is `ColumnImpl.fromDateDouble`, i.e. the local
    /// wall clock time minus the zone offset.  `LOCAL_DATE_TIME` reads the same
    /// bytes as the `LocalDateTime`.
    #[test]
    fn date_type_read_conversions_match_java() {
        let tz = TimeZone::from_offset_minutes(480);
        let ldt = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0);
        let double = datenums::to_date_double(ldt);
        let bits = double.to_bits();

        let read = read_short_date_time(bits, DateTimeType::Date);
        assert_eq!(read, Value::DateBits(double));
        assert_eq!(
            date_value_millis(&read, &tz),
            Some(ldt.to_epoch_millis() - 480 * 60_000),
            "DateExt.getTime() is the stored wall clock time minus the offset"
        );
        // the original bits survive a write in both modes
        assert_eq!(
            date_value_bits_with(&read, &tz, DateTimeType::Date).unwrap(),
            bits
        );
        assert_eq!(
            date_value_bits_with(&read, &tz, DateTimeType::LocalDateTime).unwrap(),
            bits
        );

        let read = read_short_date_time(bits, DateTimeType::LocalDateTime);
        assert_eq!(read, Value::DateTime(ldt));
        assert_eq!(date_value_millis(&read, &tz), None);
    }

    /// The `DATE` read/write round trip is lossless for the pre-1900 values
    /// `oldDatesV2007.accdb` holds (the negative OLE doubles): the `DateExt`
    /// keeps the exact bits `writeDateValue` stores again.
    #[test]
    fn date_type_round_trip_keeps_an_old_date_byte_identical() {
        let tz = TimeZone::from_offset_minutes(480);
        for ldt in [
            LocalDateTime::of_date(1582, 10, 15),
            LocalDateTime::of_date(1582, 10, 14),
            LocalDateTime::of_date(1492, 1, 10),
            LocalDateTime::of_date(1392, 1, 10),
        ] {
            let double = datenums::to_date_double(ldt);
            let bits = double.to_bits();
            let read = read_short_date_time(bits, DateTimeType::Date);
            // the LDT view of the same bits is the original date
            assert_eq!(
                ldt_from_local_date_double(double),
                ldt,
                "the stored value is {ldt:?}"
            );
            // the Date view is the millisecond rounded instant
            let millis = date_value_millis(&read, &tz).unwrap();
            assert_eq!(millis, ldt.to_epoch_millis() - 480 * 60_000);
            // writing the read value back reproduces the bits
            assert_eq!(
                date_value_bits_with(&read, &tz, DateTimeType::Date).unwrap(),
                bits
            );
        }
    }

    /// `DefaultDateTimeFactory.toDateDouble` vs `LDTDateTimeFactory
    /// .toDateDouble`: a `LocalDateTime` goes through
    /// `atZone(zone).toInstant()` in `DATE` mode, which drops the
    /// sub-millisecond part; a numeric value is a `java.util.Date` instant and
    /// is converted identically by both factories for a fixed offset zone.
    #[test]
    fn date_type_write_conversions_match_the_java_factories() {
        let tz = TimeZone::from_offset_minutes(480);
        let sub_ms = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_456_789);
        let millis_ldt = LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000);

        // LOCAL_DATE_TIME keeps the full nanosecond precision
        let ldt_bits =
            date_value_bits_with(&Value::DateTime(sub_ms), &tz, DateTimeType::LocalDateTime)
                .unwrap();
        assert_eq!(ldt_bits, datenums::to_date_double(sub_ms).to_bits());

        // DATE truncates to whole milliseconds (via Instant.toEpochMilli)
        let date_bits =
            date_value_bits_with(&Value::DateTime(sub_ms), &tz, DateTimeType::Date).unwrap();
        assert_eq!(
            date_bits,
            datenums::to_date_double(millis_ldt).to_bits(),
            "DATE mode writes the java.util.Date milliseconds"
        );
        assert_ne!(date_bits, ldt_bits);

        // a value which is already on a millisecond boundary is the same in
        // both modes (which is why the mode changes no existing bytes)
        assert_eq!(
            date_value_bits_with(&Value::DateTime(millis_ldt), &tz, DateTimeType::Date).unwrap(),
            date_value_bits_with(
                &Value::DateTime(millis_ldt),
                &tz,
                DateTimeType::LocalDateTime
            )
            .unwrap()
        );

        // a numeric value is a java.util.Date epoch instant in both modes
        let instant = 1_584_246_645_000i64; // 2020-03-15T10:30:45Z
        let date_bits =
            date_value_bits_with(&Value::BigInt(instant), &tz, DateTimeType::Date).unwrap();
        let ldt_bits =
            date_value_bits_with(&Value::BigInt(instant), &tz, DateTimeType::LocalDateTime)
                .unwrap();
        assert_eq!(date_bits, ldt_bits);
        assert_eq!(
            date_bits,
            datenums::to_date_double(tz.local_date_time_from_millis(instant)).to_bits()
        );
    }

    /// `datenums::to_date_double_from_millis` is
    /// `ColumnImpl.toDateDouble(Object, dtc)` for a `Date` (and the inverse of
    /// `from_date_double` for a millisecond value).
    #[test]
    fn date_millis_conversion_is_the_inverse_of_the_date_read() {
        let tz = TimeZone::from_offset_minutes(480);
        for millis in [
            1_584_246_645_000i64, // 2020-03-15T10:30:45Z
            -12_219_292_800_000,  // 1582-10-15T00:00:00Z
            0,
            -1,
        ] {
            let double = datenums::to_date_double_from_millis(millis, &tz);
            assert_eq!(
                datenums::from_date_double(double, &tz),
                millis,
                "round trip for {millis}"
            );
        }
        // a UTC database zone stores the instant unchanged
        let utc = TimeZone::utc();
        let double = datenums::to_date_double_from_millis(-1, &utc);
        assert_eq!(datenums::from_date_double(double, &utc), -1);
    }

    /// For a value which is a whole number of milliseconds the two
    /// `DateTimeFactory` write conversions store exactly the same OLE double,
    /// which is why `DateTimeType` only changes the bytes of a sub-millisecond
    /// date/time (and why the mode can be switched without rewriting the
    /// existing rows).
    #[test]
    fn both_date_time_types_store_the_same_bits_for_whole_milliseconds() {
        let mut checked = 0usize;
        // 1900-01-01 .. 2099-12-31, several times of day and sub-second parts
        for day in 0..73000i64 {
            let date = LocalDateTime::from_epoch_day(day);
            for (h, m, s, n) in [
                (0u32, 0u32, 0u32, 0u32),
                (12, 30, 45, 0),
                (23, 59, 59, 0),
                (8, 9, 10, 123_000_000),
                (1, 2, 3, 999_000_000),
                (5, 6, 7, 500_000_000),
            ] {
                let ldt = LocalDateTime::new(date.year, date.month, date.day, h, m, s, n);
                let tz = TimeZone::from_offset_minutes(480);
                let local =
                    date_value_bits_with(&Value::DateTime(ldt), &tz, DateTimeType::Date).unwrap();
                let ldt_bits =
                    date_value_bits_with(&Value::DateTime(ldt), &tz, DateTimeType::LocalDateTime)
                        .unwrap();
                assert_eq!(
                    local, ldt_bits,
                    "the two factories disagree for {ldt:?} ({local:016x} vs {ldt_bits:016x})"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 73000 * 6);
    }

    /// `DatabaseImpl.getDateForObject` reads a catalog date with
    /// `DateTimeType.DATE` and converts the `DateExt` to a `LocalDateTime`
    /// (`ColumnImpl.toLocalDateTime`).  The `Date.getTime()` rounds the stored
    /// double to a whole millisecond first, which is visible in real files
    /// (the V2003 `Table2` update date is `.457001` in LOCAL_DATE_TIME mode but
    /// `.457` in DATE mode).
    #[test]
    fn date_ext_local_date_time_rounds_like_java() {
        let tz = TimeZone::from_offset_minutes(480);
        let sub_ms = LocalDateTime::new(2006, 8, 3, 21, 57, 4, 457_001_000);
        let bits = datenums::to_date_double(sub_ms);
        assert_eq!(
            date_ext_local_date_time(bits, &tz),
            LocalDateTime::new(2006, 8, 3, 21, 57, 4, 457_000_000)
        );
        // a whole millisecond value is unchanged
        let whole = LocalDateTime::new(2009, 11, 14, 7, 49, 16, 804_000_000);
        assert_eq!(
            date_ext_local_date_time(datenums::to_date_double(whole), &tz),
            whole
        );
        // a fixed offset zone makes the two conversions cancel exactly
        assert_eq!(
            date_ext_local_date_time(bits, &tz),
            ldt_from_local_date_double(bits).with_nano(457_000_000)
        );
    }

    // -----------------------------------------------------------------
    // the Java `Column` object API (getTable/getDatabase/row values/...)
    // -----------------------------------------------------------------

    fn objapi_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn create_objapi_db(name: &str) -> (std::path::PathBuf, Database) {
        use crate::format::FileFormat;
        let path = objapi_tmp_dir().join(format!(
            "jackcess_column_objapi_{}_{}.accdb",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2007, &path).expect("create database");
        db.now_ms = Some(1_600_000_000_000);
        (path, db)
    }

    #[test]
    fn java_value_accessors_match_the_definition() {
        use crate::datatype::sql_types;
        let format = JetFormat::version_4();

        let mut link = column("Link", DataType::Memo, None);
        link.column_id = 7;
        link.column_index = 2;
        link.column_length = 20;
        link.flags = HYPERLINK_FLAG_MASK;
        assert!(link.is_hyperlink());
        assert!(!link.is_security_identifier());
        assert_eq!(link.get_column_id(), 7);
        assert_eq!(link.get_column_index(), 2);
        // `MemoColumnImpl.getSQLType` is `DataType.LONGVARCHAR`
        assert_eq!(link.get_sql_type().unwrap(), sql_types::LONGVARCHAR);
        // `getLengthInUnits` returns the (cached) length as a `short`
        link.length_in_units = 40;
        assert_eq!(link.get_length_in_units(&format), 40);
        assert_eq!(
            link.get_length_in_units(&format) as i32,
            link.length_in_units(&format)
        );
        // a column without a cache computes it from the type and the format,
        // exactly like `length_in_units`
        let mut computed = column("T", DataType::Text, None);
        computed.column_length = 20;
        computed.length_in_units = -1;
        assert_eq!(
            computed.get_length_in_units(&format) as i32,
            computed.length_in_units(&format)
        );

        // only a MEMO column can be a hyperlink
        let mut text = column("Link", DataType::Text, None);
        text.flags = HYPERLINK_FLAG_MASK;
        assert!(!text.is_hyperlink());
        let mut secured = column("SID", DataType::Binary, None);
        secured.flags = SECURITY_IDENTIFIER_FLAG_MASK;
        assert!(secured.is_security_identifier());
        assert!(!secured.is_hyperlink());

        // a data type without a JDBC/SQL type is Java's `JackcessException`
        // (whose message embeds a non deterministic identity hash there)
        let guid = column("G", DataType::Guid, None);
        assert_eq!(
            guid.get_sql_type().unwrap_err().to_string(),
            "Unsupported data type: GUID"
        );
        assert_eq!(
            column("B", DataType::Boolean, None).get_sql_type().unwrap(),
            sql_types::BOOLEAN
        );
    }

    #[test]
    fn row_value_accessors_match_java() {
        use std::collections::BTreeMap;
        let mut col = column("Num", DataType::Long, None);
        col.column_index = 1;

        // `getRowValue(Object[])` indexes the row array by column index
        let mut row = vec![Value::Int(1), Value::Long(2), Value::Long(3)];
        assert_eq!(col.get_row_value(&row), Some(&Value::Long(2)));
        assert!(col.get_row_value(&row[..1]).is_none());
        // `setRowValue` returns the stored value, not the previous one
        assert_eq!(
            col.set_row_value(&mut row, Value::Long(42)).unwrap(),
            Value::Long(42)
        );
        assert_eq!(row[1], Value::Long(42));
        assert!(matches!(
            col.set_row_value(&mut row[..1], Value::Long(1)),
            Err(Error::IllegalArgument(_))
        ));

        // `getRowValue(Map)` / `setRowValue(Map)` use the exact column name
        let mut map = BTreeMap::new();
        map.insert("Num".to_string(), Value::Long(5));
        assert_eq!(col.get_row_value_map(&map), Some(&Value::Long(5)));
        assert!(col.get_row_value_map(&BTreeMap::new()).is_none());
        assert_eq!(
            col.set_row_value_map(&mut map, Value::Long(6)),
            Value::Long(6)
        );
        assert_eq!(map["Num"], Value::Long(6));
        assert_eq!(col.get_row_value_map(&map), Some(&Value::Long(6)));
    }

    #[test]
    fn table_and_database_context_are_verified() {
        let (path, mut db) = create_objapi_db("context");
        let spec = crate::tablecreator::TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        );
        db.create_table(&spec).expect("create T");
        let other =
            crate::tablecreator::TableSpec::new("U", vec![ColumnSpec::new("ID", DataType::Long)]);
        db.create_table(&other).expect("create U");

        let page;
        let column;
        {
            let table = db.get_table("T").unwrap();
            page = table.table_def_page_number;
            column = table.get_column("Name").unwrap().clone();
            // `Column.getTable` returns the owning table
            assert_eq!(column.get_table(table).unwrap().get_name(), "T");
        }
        // an unrelated table is rejected
        let unrelated = db.get_table("U").unwrap();
        assert!(matches!(
            column.get_table(unrelated),
            Err(Error::IllegalArgument(_))
        ));
        // `Column.getDatabase` is the owning table's database
        assert!(column
            .get_database(&mut db)
            .get_table_names()
            .unwrap()
            .contains(&"T".to_string()));
        // `Column.getProperties` returns the (empty, named) column map
        let props = column.get_properties(&mut db, page).expect("properties");
        assert_eq!(props.name(), "Name");
        assert!(props.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn complex_and_version_history_links_resolve_through_the_database() {
        let (path, mut db) = create_objapi_db("complex");
        let spec = crate::tablecreator::TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Tags", DataType::Text)
                    .set_multi_value(DataType::Text)
                    .set_value_list(&["a", "b"]),
                ColumnSpec::new("Body", DataType::Memo).set_append_only(true),
            ],
        );
        db.create_table(&spec).expect("create T");

        let tags = db
            .get_table("T")
            .unwrap()
            .get_column("Tags")
            .unwrap()
            .clone();
        let info = tags
            .get_complex_info(&mut db, "T")
            .expect("complex info")
            .expect("Tags is a complex column");
        assert_eq!(info.kind, crate::complex::ComplexDataType::MultiValue);
        assert_eq!(info.value_column.as_deref(), Some("Value"));

        let body = db
            .get_table("T")
            .unwrap()
            .get_column("Body")
            .unwrap()
            .clone();
        let version_history = body
            .get_version_history_column(&mut db, "T")
            .expect("version history lookup")
            .expect("Body is append only");
        assert_eq!(version_history.data_type, DataType::ComplexType);
        assert_eq!(
            version_history
                .get_complex_info(&mut db, "T")
                .expect("complex info")
                .expect("version history column is complex")
                .kind,
            crate::complex::ComplexDataType::VersionHistory
        );

        // a plain column has neither
        let id = db.get_table("T").unwrap().get_column("ID").unwrap().clone();
        assert!(id.get_complex_info(&mut db, "T").unwrap().is_none());
        assert!(id
            .get_version_history_column(&mut db, "T")
            .unwrap()
            .is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn column_validator_slot_matches_java() {
        use crate::validator::{ColumnValidator, ColumnValidatorContext, ColumnValidatorHandle};

        struct PlusOne;
        impl ColumnValidator for PlusOne {
            fn validate(
                &self,
                _context: &ColumnValidatorContext<'_>,
                value: Value,
            ) -> Result<Value> {
                Ok(match value {
                    Value::Long(value) => Value::Long(value + 1),
                    other => other,
                })
            }

            fn description(&self) -> String {
                "plus-one".to_string()
            }
        }

        let mut col = column("C", DataType::Long, None);
        // `ColumnImpl` starts with the value preserving `SimpleColumnValidator`
        assert!(col.get_column_validator().is_simple());
        // `setColumnValidator` stores the user ("external") validator
        col.set_column_validator(Some(ColumnValidatorHandle::new(PlusOne)));
        assert_eq!(col.get_column_validator().description(), "plus-one");
        assert_eq!(
            col.get_column_validator()
                .validate(&ColumnValidatorContext::new(&col), Value::Long(1))
                .unwrap(),
            Value::Long(2)
        );
        // Java's `setColumnValidator(null)` restores the default
        col.set_column_validator(None);
        assert!(col.get_column_validator().is_simple());
    }
}

// ---------------------------------------------------------------------------
// Jet 3 text decoding (the file's code page, not unicode)
// ---------------------------------------------------------------------------
#[cfg(test)]
mod jet3_text_tests {
    use super::*;

    /// `decodeUncompressedText` with the windows-1252 code page: the bytes
    /// jackcess' `new String(bytes, "windows-1252")` maps, and the five
    /// positions that charset leaves undefined (which Java reports as the
    /// replacement character).
    #[test]
    fn windows_1252_decoding_matches_the_java_charset() {
        let charset = crate::format::Charset::Cp1252;
        // plain ASCII
        assert_eq!(decode_uncompressed_text(b"abc", 0, 3, charset), "abc");
        // the defined 0x80..0x9F positions
        assert_eq!(
            decode_uncompressed_text(&[0x80, 0x93, 0x94, 0x92], 0, 4, charset),
            "\u{20ac}\u{201c}\u{201d}\u{2019}"
        );
        // 0xA0..0xFF are latin-1
        assert_eq!(
            decode_uncompressed_text(&[0xa3, 0xe9, 0xff], 0, 3, charset),
            "\u{a3}\u{e9}\u{ff}"
        );
        // the undefined positions become U+FFFD, exactly like Java's decoder
        for byte in [0x81u8, 0x8d, 0x8f, 0x90, 0x9d] {
            assert_eq!(
                decode_uncompressed_text(&[byte], 0, 1, charset),
                "\u{fffd}",
                "byte {:#04x}",
                byte
            );
        }
        // a Jet 3 text value which contains one of them, embedded
        assert_eq!(
            decode_uncompressed_text(&[b'A', b'A', 0x81, b'A', b'A'], 0, 5, charset),
            "AA\u{fffd}AA"
        );
    }

    /// The encode side is the inverse on the defined positions, and writes '?'
    /// for a code point windows-1252 cannot encode (Java's REPLACE action).
    #[test]
    fn windows_1252_encoding_matches_the_java_charset() {
        let charset = crate::format::Charset::Cp1252;
        assert_eq!(encode_uncompressed_text("abc", charset), b"abc");
        assert_eq!(
            encode_uncompressed_text("\u{20ac}\u{201c}\u{201d}", charset),
            vec![0x80, 0x93, 0x94]
        );
        assert_eq!(encode_uncompressed_text("\u{4e2d}", charset), b"?");
    }
}
