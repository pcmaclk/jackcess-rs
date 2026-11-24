//! Port of `com.healthmarketscience.jackcess.DataType`.
//!
//! # Java name mapping
//!
//! | Java (`com.healthmarketscience.jackcess.DataType`) | Rust |
//! |---|---|
//! | `values()` | [`DataType::values`] (same as [`DataType::all`]) |
//! | `fromByte(byte)` | [`DataType::from_byte`] (`Option`) / [`DataType::try_from_byte`] (`IOException` equivalent) |
//! | `getValue()` | [`DataType::value`] |
//! | `getSQLType()` | [`DataType::get_sql_type`] (`Option`, Java throws) / [`DataType::try_get_sql_type`] |
//! | `getFixedSize()`, `getFixedSize(Short)` | [`DataType::get_fixed_size`], [`DataType::get_fixed_size_with_length`] (0 when Java throws) / [`DataType::try_get_fixed_size`], [`DataType::try_get_fixed_size_with_length`] |
//! | `getUnitSize()`, `getUnitSize(JetFormat)` | [`DataType::get_unit_size`], [`DataType::get_unit_size_format`] (also [`DataType::get_unit_size_with`]) |
//! | `toUnitSize(int[, JetFormat])` | [`DataType::to_unit_size`] |
//! | `fromUnitSize(int[, JetFormat])` | [`DataType::from_unit_size`] |
//! | `isValidSize/Scale/Precision` | [`DataType::is_valid_size`] / [`DataType::is_valid_scale`] / [`DataType::is_valid_precision`] |
//! | `toValidSize/Scale/Precision` | [`DataType::to_valid_size`] / [`DataType::to_valid_scale`] / [`DataType::to_valid_precision`] |
//! | `hasScalePrecision` (`getHasScalePrecision()`) | [`DataType::has_scale_precision`] |
//! | `isMultipleAutoNumberAllowed()` | [`DataType::is_multiple_auto_number_allowed`] |
//! | `fromSQLType(int)` | [`DataType::from_sql_type`] |
//! | `fromSQLType(int, int)` | [`DataType::from_sql_type_with_length`] |
//! | `fromSQLType(int, int, Database.FileFormat)` | [`DataType::from_sql_type_with_format`] |
//!
//! Java's overloads become distinct Rust names because Rust has no overloading;
//! the `fromSQLType` family returns `Result` (Java throws `IOException` /
//! `JackcessException`) instead of silently yielding `None`, so the exact Java
//! failure text ("Unsupported SQL type: N") can be reproduced.
//!
//! The existing [`DataType::get_type`] / [`DataType::get_type_with_length`]
//! helpers are the port's original `Option` based lookups used by the column
//! reader; they are kept unchanged for compatibility.  The new
//! `from_sql_type*` methods implement the full Java mapping (including the
//! "alternate type" rule for values which are too long).

/// The Access column types, mirroring the Java enum constants exactly
/// (including their internal byte values).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum DataType {
    Boolean,
    Byte,
    Int,
    Long,
    Money,
    Float,
    Double,
    ShortDateTime,
    Binary,
    Text,
    Ole,
    Memo,
    Guid,
    Numeric,
    BigBinary,
    ComplexType,
    BigInt,
    ExtDateTime,
    UnsupportedFixedLen,
    UnsupportedVarLen,
}

/// java.sql.Types value used by the port (only the ones jackcess references).
pub mod sql_types {
    pub const BIT: i32 = -7;
    pub const TINYINT: i32 = -6;
    pub const SMALLINT: i32 = 5;
    pub const INTEGER: i32 = 4;
    pub const BIGINT: i32 = -5;
    pub const DECIMAL: i32 = 3;
    pub const NUMERIC: i32 = 2;
    pub const FLOAT: i32 = 6;
    pub const REAL: i32 = 7;
    pub const DOUBLE: i32 = 8;
    pub const CHAR: i32 = 1;
    pub const VARCHAR: i32 = 12;
    pub const LONGVARCHAR: i32 = -1;
    pub const DATE: i32 = 91;
    pub const TIME: i32 = 92;
    pub const TIMESTAMP: i32 = 93;
    pub const BINARY: i32 = -2;
    pub const VARBINARY: i32 = -3;
    pub const LONGVARBINARY: i32 = -4;
    pub const BLOB: i32 = 2004;
    pub const CLOB: i32 = 2005;
    pub const BOOLEAN: i32 = 16;
    // the types added after JDK 1.5, which `DataType.addNewSqlType` looks up by
    // reflection (`SqlHelperImpl.getNewSqlType`); their `java.sql.Types` values
    // are fixed by the JDBC specification.
    pub const NCHAR: i32 = -15;
    pub const NVARCHAR: i32 = -9;
    pub const LONGNVARCHAR: i32 = -16;
    pub const NCLOB: i32 = 2011;
    pub const TIME_WITH_TIMEZONE: i32 = 2013;
    pub const TIMESTAMP_WITH_TIMEZONE: i32 = 2014;
}

#[derive(Clone, Copy, Debug)]
pub struct TypeInfo {
    pub value: u8,
    pub sql_type: Option<i32>,
    pub fixed_size: Option<i32>,
    pub variable_length: bool,
    pub long_value: bool,
    pub min_size: i32,
    pub default_size: i32,
    pub max_size: i32,
    pub has_scale_precision: bool,
    pub min_scale: i32,
    pub default_scale: i32,
    pub max_scale: i32,
    pub min_precision: i32,
    pub default_precision: i32,
    pub max_precision: i32,
    pub unit_size: i32,
}

impl DataType {
    pub fn all() -> &'static [DataType] {
        use DataType::*;
        &[
            Boolean,
            Byte,
            Int,
            Long,
            Money,
            Float,
            Double,
            ShortDateTime,
            Binary,
            Text,
            Ole,
            Memo,
            Guid,
            Numeric,
            BigBinary,
            ComplexType,
            BigInt,
            ExtDateTime,
            UnsupportedFixedLen,
            UnsupportedVarLen,
        ]
    }

    pub fn info(self) -> TypeInfo {
        use sql_types::*;
        use DataType::*;
        // (value, sqlType, fixedSize, variableLength, longValue, minSize,
        //  defaultSize, maxSize, hasScalePrecision, minScale, defaultScale,
        //  maxScale, minPrecision, defaultPrecision, maxPrecision, unitSize)
        match self {
            Boolean => ti(0x01, Some(BOOLEAN), Some(1)),
            Byte => ti(0x02, Some(TINYINT), Some(1)),
            Int => ti(0x03, Some(SMALLINT), Some(2)),
            Long => ti(0x04, Some(INTEGER), Some(4)),
            Money => {
                let mut t = ti(0x05, Some(DECIMAL), Some(8));
                t.min_scale = 4;
                t.default_scale = 4;
                t.max_scale = 4;
                t.min_precision = 19;
                t.default_precision = 19;
                t.max_precision = 19;
                t
            }
            Float => ti(0x06, Some(FLOAT), Some(4)),
            Double => ti(0x07, Some(DOUBLE), Some(8)),
            ShortDateTime => ti(0x08, Some(TIMESTAMP), Some(8)),
            Binary => {
                let mut t = ti(0x09, Some(BINARY), None);
                t.variable_length = true;
                t.default_size = 255;
                t.max_size = 255;
                t
            }
            Text => {
                let mut t = ti(0x0A, Some(VARCHAR), None);
                t.variable_length = true;
                t.default_size = crate::format::TEXT_FIELD_MAX_LENGTH as i32;
                t.max_size = crate::format::TEXT_FIELD_MAX_LENGTH as i32;
                t.unit_size = crate::format::TEXT_FIELD_UNIT_SIZE as i32;
                t
            }
            Ole => {
                let mut t = ti(0x0B, Some(LONGVARBINARY), None);
                t.variable_length = true;
                t.long_value = true;
                t.max_size = 0x3FFF_FFFF;
                t
            }
            Memo => {
                let mut t = ti(0x0C, Some(LONGVARCHAR), None);
                t.variable_length = true;
                t.long_value = true;
                t.max_size = 0x3FFF_FFFF;
                t.unit_size = crate::format::TEXT_FIELD_UNIT_SIZE as i32;
                t
            }
            Guid => ti(0x0F, None, Some(16)),
            Numeric => {
                let mut t = ti(0x10, Some(NUMERIC), Some(17));
                t.variable_length = true;
                t.min_size = 17;
                t.default_size = 17;
                t.max_size = 17;
                t.has_scale_precision = true;
                t.min_scale = 0;
                t.default_scale = 0;
                t.max_scale = 28;
                t.min_precision = 1;
                t.default_precision = 18;
                t.max_precision = 28;
                t
            }
            BigBinary => ti(0x11, None, Some(3992)),
            ComplexType => ti(0x12, None, Some(4)),
            BigInt => ti(0x13, Some(BIGINT), Some(8)),
            ExtDateTime => ti(0x14, None, Some(42)),
            UnsupportedFixedLen => ti(0xFE, None, None),
            UnsupportedVarLen => {
                let mut t = ti(0xFF, None, None);
                t.variable_length = true;
                t.max_size = 0x3FFF_FFFF;
                t
            }
        }
    }

    pub fn value(self) -> u8 {
        self.info().value
    }

    pub fn is_variable_length(self) -> bool {
        self.info().variable_length
    }

    /// Some "var len" fields do not really have a variable length, e.g. NUMERIC.
    pub fn is_true_variable_length(self) -> bool {
        let i = self.info();
        i.variable_length && i.min_size != i.max_size
    }

    pub fn is_long_value(self) -> bool {
        self.info().long_value
    }

    pub fn get_sql_type(self) -> Option<i32> {
        self.info().sql_type
    }

    /// `getFixedSize()`, returns 0 when java would throw.
    pub fn get_fixed_size(self) -> i32 {
        self.info().fixed_size.unwrap_or(0)
    }

    pub fn has_fixed_size(self) -> bool {
        self.info().fixed_size.is_some()
    }

    /// `getFixedSize(Short colLength)`
    pub fn get_fixed_size_with_length(self, col_length: Option<i16>) -> i32 {
        let info = self.info();
        if let Some(fixed) = info.fixed_size {
            if let Some(len) = col_length {
                return fixed.max(len as i32);
            }
            return fixed;
        }
        if let Some(len) = col_length {
            return len as i32;
        }
        0
    }

    pub fn is_textual(self) -> bool {
        matches!(self, DataType::Text | DataType::Memo)
    }

    pub fn get_unit_size_format(self, format: Option<&crate::format::JetFormat>) -> i32 {
        if let Some(f) = format {
            if self.is_textual() {
                return f.size_text_field_unit as i32;
            }
        }
        self.info().unit_size
    }

    pub fn to_unit_size(self, size: i32, format: Option<&crate::format::JetFormat>) -> i32 {
        size / self.get_unit_size_format(format)
    }

    pub fn from_unit_size(self, unit_size: i32, format: Option<&crate::format::JetFormat>) -> i32 {
        unit_size * self.get_unit_size_format(format)
    }

    pub fn may_be_auto_number(self) -> bool {
        matches!(
            self,
            DataType::Long | DataType::Guid | DataType::ComplexType
        )
    }

    pub fn get_min_size(self) -> i32 {
        self.info().min_size
    }

    pub fn get_default_size(self) -> i32 {
        self.info().default_size
    }

    pub fn get_max_size(self) -> i32 {
        self.info().max_size
    }

    pub fn get_min_scale(self) -> i32 {
        self.info().min_scale
    }

    pub fn get_default_scale(self) -> i32 {
        self.info().default_scale
    }

    pub fn get_max_scale(self) -> i32 {
        self.info().max_scale
    }

    pub fn get_min_precision(self) -> i32 {
        self.info().min_precision
    }

    pub fn get_default_precision(self) -> i32 {
        self.info().default_precision
    }

    pub fn get_max_precision(self) -> i32 {
        self.info().max_precision
    }

    pub fn has_scale_precision(self) -> bool {
        self.info().has_scale_precision
    }

    pub fn get_unit_size(self) -> i32 {
        self.info().unit_size
    }

    pub fn is_unsupported(self) -> bool {
        matches!(
            self,
            DataType::UnsupportedFixedLen | DataType::UnsupportedVarLen
        )
    }

    /// `DataType.values()`: every constant in declaration order, unsupported
    /// placeholders included (the same order as [`DataType::all`]).
    pub fn values() -> &'static [DataType] {
        DataType::all()
    }

    /// `DataType.isMultipleAutoNumberAllowed()`: only `COMPLEX_TYPE` may be
    /// autonumbered more than once per table.
    pub fn is_multiple_auto_number_allowed(self) -> bool {
        self == DataType::ComplexType
    }

    /// `DataType.isValidSize(int)`.
    pub fn is_valid_size(self, size: i32) -> bool {
        is_within_range(size, self.get_min_size(), self.get_max_size())
    }

    /// `DataType.isValidScale(int)`.
    pub fn is_valid_scale(self, scale: i32) -> bool {
        is_within_range(scale, self.get_min_scale(), self.get_max_scale())
    }

    /// `DataType.isValidPrecision(int)`.
    pub fn is_valid_precision(self, precision: i32) -> bool {
        is_within_range(
            precision,
            self.get_min_precision(),
            self.get_max_precision(),
        )
    }

    /// `DataType.toValidSize(int)`: clamps to the type's size range.
    pub fn to_valid_size(self, size: i32) -> i32 {
        to_valid_range(size, self.get_min_size(), self.get_max_size())
    }

    /// `DataType.toValidScale(int)`.
    pub fn to_valid_scale(self, scale: i32) -> i32 {
        to_valid_range(scale, self.get_min_scale(), self.get_max_scale())
    }

    /// `DataType.toValidPrecision(int)`.
    pub fn to_valid_precision(self, precision: i32) -> i32 {
        to_valid_range(
            precision,
            self.get_min_precision(),
            self.get_max_precision(),
        )
    }

    /// `DataType.getSQLType()`: the JDBC type, or the `JackcessException`
    /// ("Unsupported data type: NAME") Java throws when the type has none.
    pub fn try_get_sql_type(self) -> crate::Result<i32> {
        match self.get_sql_type() {
            Some(sql_type) => Ok(sql_type),
            None => Err(crate::Error::Jackcess(format!(
                "Unsupported data type: {}",
                self.get_name()
            ))),
        }
    }

    /// `DataType.getFixedSize()`: the Java failure for the variable length
    /// types is an `IllegalArgumentException`
    /// ("Unexpected fixed length column NAME").  [`DataType::get_fixed_size`]
    /// keeps the port's original 0 fallback.
    pub fn try_get_fixed_size(self) -> crate::Result<i32> {
        self.try_get_fixed_size_with_length(None)
    }

    /// `DataType.getFixedSize(Short colLength)`, with the Java exception for
    /// types which have no fixed size and no column length.
    pub fn try_get_fixed_size_with_length(self, col_length: Option<i16>) -> crate::Result<i32> {
        if self.has_fixed_size() || col_length.is_some() {
            return Ok(self.get_fixed_size_with_length(col_length));
        }
        Err(crate::Error::IllegalArgument(format!(
            "Unexpected fixed length column {}",
            self.get_name()
        )))
    }

    /// `DataType.getUnitSize(JetFormat)`; alias of
    /// [`DataType::get_unit_size_format`] using the Java overload's name.
    pub fn get_unit_size_with(self, format: Option<&crate::format::JetFormat>) -> i32 {
        self.get_unit_size_format(format)
    }

    /// `DataType.fromByte(b)`.  The port form returns `None` for an unknown or
    /// unsupported byte value; [`DataType::try_from_byte`] returns the Java
    /// `IOException` ("Unrecognized data type: N", N signed).
    pub fn from_byte(b: u8) -> Option<DataType> {
        DataType::all()
            .iter()
            .copied()
            .find(|t| !t.is_unsupported() && t.value() == b)
    }

    /// `DataType.fromByte(byte)` with the Java failure.
    pub fn try_from_byte(b: u8) -> crate::Result<DataType> {
        match DataType::from_byte(b) {
            Some(t) => Ok(t),
            None => Err(crate::Error::Jackcess(format!(
                "Unrecognized data type: {}",
                b as i8
            ))),
        }
    }

    /// `DataType.getType(int sqlType)` (only the direct mappings).
    pub fn get_type(sql_type: i32) -> Option<DataType> {
        use sql_types::*;
        let direct = DataType::all()
            .iter()
            .copied()
            .find(|t| t.get_sql_type() == Some(sql_type));
        if direct.is_some() {
            return direct;
        }
        match sql_type {
            BIT => Some(DataType::Byte),
            BLOB => Some(DataType::Ole),
            CLOB => Some(DataType::Memo),
            BIGINT => Some(DataType::Long),
            CHAR => Some(DataType::Text),
            DATE => Some(DataType::ShortDateTime),
            REAL => Some(DataType::Double),
            TIME => Some(DataType::ShortDateTime),
            VARBINARY => Some(DataType::Binary),
            _ => None,
        }
    }

    /// `DataType.getType(int sqlType, Integer lengthInUnits)`.
    pub fn get_type_with_length(sql_type: i32, length_in_units: Option<i32>) -> Option<DataType> {
        let data_type = DataType::get_type(sql_type)?;
        if let Some(len) = length_in_units {
            if data_type.get_max_size() < len * data_type.get_unit_size() {
                // this may be a function of the length
                let alt = match sql_type {
                    sql_types::VARCHAR => Some(DataType::Memo),
                    sql_types::VARBINARY | sql_types::BINARY => Some(DataType::Ole),
                    _ => None,
                };
                return alt.or(Some(data_type));
            }
        }
        Some(data_type)
    }

    /// `DataType.fromSQLType(int sqlType)`.
    pub fn from_sql_type(sql_type: i32) -> crate::Result<DataType> {
        DataType::from_sql_type_full(sql_type, 0, None)
    }

    /// `DataType.fromSQLType(int sqlType, int lengthInUnits)`.
    pub fn from_sql_type_with_length(
        sql_type: i32,
        length_in_units: i32,
    ) -> crate::Result<DataType> {
        DataType::from_sql_type_full(sql_type, length_in_units, None)
    }

    /// `DataType.fromSQLType(int sqlType, int lengthInUnits,
    /// Database.FileFormat fileFormat)`.
    pub fn from_sql_type_with_format(
        sql_type: i32,
        length_in_units: i32,
        file_format: Option<crate::format::FileFormat>,
    ) -> crate::Result<DataType> {
        DataType::from_sql_type_full(sql_type, length_in_units, file_format)
    }

    /// The full `DataType.fromSQLType` mapping.  `file_format` corresponds to
    /// Java's nullable `FileFormat` argument
    /// (`DatabaseImpl.getFileFormatDetails(fileFormat).getFormat()`); `None`
    /// keeps the format independent behaviour.
    pub fn from_sql_type_full(
        sql_type: i32,
        length_in_units: i32,
        file_format: Option<crate::format::FileFormat>,
    ) -> crate::Result<DataType> {
        let candidates = sql_type_candidates(sql_type)
            .ok_or_else(|| crate::Error::Jackcess(format!("Unsupported SQL type: {sql_type}")))?;
        let format = file_format.map(format_for_file_format);
        let mut data_type = candidates[0];
        if candidates.len() > 1 {
            if let Some(file_format) = file_format {
                // multiple possibilities, ordered from lowest version to the
                // highest version supported: go in opposite order to find the
                // best type for this format
                for candidate in candidates.iter().rev() {
                    if file_format_supports_data_type(file_format, *candidate) {
                        data_type = *candidate;
                        break;
                    }
                }
            }
        }

        // make sure the size is reasonable
        let size = data_type.from_unit_size(length_in_units, format.as_ref());
        if data_type.is_variable_length() && !data_type.is_valid_size(size) {
            // try the alternate type; alternate "long value" types are always
            // accepted regardless of the given lengthInUnits
            if let Some(alt) = alternate_sql_type(sql_type) {
                if alt.is_long_value() || alt.is_valid_size(size) {
                    data_type = alt;
                }
            }
        }
        Ok(data_type)
    }

    pub fn get_name(self) -> &'static str {
        match self {
            DataType::Boolean => "BOOLEAN",
            DataType::Byte => "BYTE",
            DataType::Int => "INT",
            DataType::Long => "LONG",
            DataType::Money => "MONEY",
            DataType::Float => "FLOAT",
            DataType::Double => "DOUBLE",
            DataType::ShortDateTime => "SHORT_DATE_TIME",
            DataType::Binary => "BINARY",
            DataType::Text => "TEXT",
            DataType::Ole => "OLE",
            DataType::Memo => "MEMO",
            DataType::Guid => "GUID",
            DataType::Numeric => "NUMERIC",
            DataType::BigBinary => "BIG_BINARY",
            DataType::ComplexType => "COMPLEX_TYPE",
            DataType::BigInt => "BIG_INT",
            DataType::ExtDateTime => "EXT_DATE_TIME",
            DataType::UnsupportedFixedLen => "UNSUPPORTED_FIXEDLEN",
            DataType::UnsupportedVarLen => "UNSUPPORTED_VARLEN",
        }
    }
}

/// `DataType.SQL_TYPES`: the candidate data types for a JDBC type, ordered from
/// the lowest version to the highest version supported (Java's static map).
fn sql_type_candidates(sql_type: i32) -> Option<&'static [DataType]> {
    use sql_types::*;
    use DataType::*;
    Some(match sql_type {
        BOOLEAN => &[Boolean],
        TINYINT => &[Byte],
        SMALLINT => &[Int],
        INTEGER => &[Long],
        // `SQL_TYPES.put(Types.BIGINT, new DataType[]{LONG, BIG_INT})`
        BIGINT => &[Long, BigInt],
        DECIMAL => &[Money],
        NUMERIC => &[Numeric],
        FLOAT => &[Float],
        REAL => &[Double],
        DOUBLE => &[Double],
        CHAR => &[Text],
        VARCHAR => &[Text],
        LONGVARCHAR => &[Memo],
        DATE => &[ShortDateTime],
        TIME => &[ShortDateTime],
        TIMESTAMP => &[ShortDateTime],
        BINARY => &[Binary],
        VARBINARY => &[Binary],
        LONGVARBINARY => &[Ole],
        BLOB => &[Ole],
        CLOB => &[Memo],
        BIT => &[Byte],
        // the JDK 1.6+ types registered by `DataType.addNewSqlType`
        NCHAR => &[Text],
        NVARCHAR => &[Text],
        LONGNVARCHAR => &[Memo],
        NCLOB => &[Memo],
        TIME_WITH_TIMEZONE => &[ShortDateTime],
        TIMESTAMP_WITH_TIMEZONE => &[ShortDateTime],
        _ => return None,
    })
}

/// `DataType.ALT_SQL_TYPES`: the larger alternative type for a value which does
/// not fit the primary type.
fn alternate_sql_type(sql_type: i32) -> Option<DataType> {
    use sql_types::*;
    Some(match sql_type {
        VARCHAR | NVARCHAR => DataType::Memo,
        VARBINARY | BINARY => DataType::Ole,
        _ => return None,
    })
}

/// `DatabaseImpl.getFileFormatDetails(fileFormat).getFormat()`.
fn format_for_file_format(file_format: crate::format::FileFormat) -> crate::format::JetFormat {
    use crate::format::FileFormat;
    match file_format {
        FileFormat::V1997 => crate::format::JetFormat::version_3(),
        FileFormat::GenericJet4 | FileFormat::V2000 | FileFormat::V2003 => {
            crate::format::JetFormat::version_4()
        }
        FileFormat::V2007 => crate::format::JetFormat::version_2007(),
        FileFormat::V2010 => crate::format::JetFormat::version_2010(),
        FileFormat::V2016 => crate::format::JetFormat::version_2016(),
        FileFormat::V2019 => crate::format::JetFormat::version_2019(),
        FileFormat::Msisam => crate::format::JetFormat::msisam(),
    }
}

/// `JetFormat.isSupportedDataType` for the format a `FileFormat` names.
///
/// The Java formats' unsupported sets are (`JetFormat`'s `V3`/`V12`/`V16`
/// sets):
///
/// * `VERSION_3` (V1997): `COMPLEX_TYPE`, `BIG_INT`, `EXT_DATE_TIME`
/// * `VERSION_4` (generic Jet 4, V2000, V2003), `VERSION_2007`,
///   `VERSION_2010` and `MSISAM`: `BIG_INT`, `EXT_DATE_TIME`
/// * `VERSION_2016`: `EXT_DATE_TIME`
/// * `VERSION_2019`: everything is supported
///
/// This is kept local to `DataType::fromSQLType` (rather than calling
/// [`crate::format::JetFormat::is_supported_data_type`]) so the SQL type
/// mapping reproduces `jetFormat.isSupportedDataType` exactly.
fn file_format_supports_data_type(
    file_format: crate::format::FileFormat,
    data_type: DataType,
) -> bool {
    use crate::format::FileFormat;
    match file_format {
        FileFormat::V1997 => !matches!(
            data_type,
            DataType::ComplexType | DataType::BigInt | DataType::ExtDateTime
        ),
        FileFormat::GenericJet4
        | FileFormat::V2000
        | FileFormat::V2003
        | FileFormat::V2007
        | FileFormat::V2010
        | FileFormat::Msisam => !matches!(data_type, DataType::BigInt | DataType::ExtDateTime),
        FileFormat::V2016 => data_type != DataType::ExtDateTime,
        FileFormat::V2019 => true,
    }
}

fn is_within_range(value: i32, min_value: i32, max_value: i32) -> bool {
    value >= min_value && value <= max_value
}

fn to_valid_range(value: i32, min_value: i32, max_value: i32) -> i32 {
    if value > max_value {
        max_value
    } else if value < min_value {
        min_value
    } else {
        value
    }
}

#[inline]
fn ti(value: u8, sql_type: Option<i32>, fixed_size: Option<i32>) -> TypeInfo {
    TypeInfo {
        value,
        sql_type,
        fixed_size,
        variable_length: false,
        long_value: false,
        min_size: 0,
        default_size: 0,
        max_size: 0,
        has_scale_precision: false,
        min_scale: 0,
        default_scale: 0,
        max_scale: 0,
        min_precision: 0,
        default_precision: 0,
        max_precision: 0,
        unit_size: 1,
    }
}

#[cfg(test)]
mod util_api_tests {
    use super::*;
    use crate::format::FileFormat;

    /// `isValidSize`/`isValidScale`/`isValidPrecision` and the `toValid*`
    /// clamps (`isWithinRange` / `toValidRange`).
    #[test]
    fn valid_ranges_match_java() {
        use DataType::*;

        assert!(Boolean.is_valid_size(0));
        assert!(!Boolean.is_valid_size(1));
        assert_eq!(Boolean.to_valid_size(5), 0);

        assert!(Text.is_valid_size(0));
        assert!(Text.is_valid_size(510));
        assert!(!Text.is_valid_size(511));
        assert!(!Text.is_valid_size(-1));
        assert_eq!(Text.to_valid_size(-1), 0);
        assert_eq!(Text.to_valid_size(1000), 510);

        assert!(Money.is_valid_scale(4));
        assert!(!Money.is_valid_scale(5));
        assert_eq!(Money.to_valid_scale(9), 4);
        assert!(Money.is_valid_precision(19));
        assert!(!Money.is_valid_precision(18));
        assert_eq!(Money.to_valid_precision(1), 19);

        assert!(Numeric.is_valid_precision(28));
        assert!(!Numeric.is_valid_precision(29));
        assert_eq!(Numeric.to_valid_precision(0), 1);
        assert!(Numeric.has_scale_precision());
        assert!(!Money.is_multiple_auto_number_allowed());
        assert!(ComplexType.is_multiple_auto_number_allowed());
        assert_eq!(DataType::values().len(), 20);
        assert_eq!(DataType::values(), DataType::all());
    }

    /// `getFixedSize` / `getUnitSize` variants.
    #[test]
    fn fixed_and_unit_sizes_match_java() {
        use DataType::*;
        assert_eq!(Long.get_fixed_size(), 4);
        assert_eq!(Long.try_get_fixed_size().unwrap(), 4);
        assert_eq!(Text.get_fixed_size(), 0);
        match Text.try_get_fixed_size() {
            Err(crate::Error::IllegalArgument(message)) => {
                assert_eq!(message, "Unexpected fixed length column TEXT")
            }
            other => panic!("expected the Java IllegalArgumentException, got {other:?}"),
        }
        assert_eq!(Text.get_fixed_size_with_length(Some(50)), 50);
        assert_eq!(Text.try_get_fixed_size_with_length(Some(50)).unwrap(), 50);
        assert_eq!(Text.get_unit_size(), 2);
        assert_eq!(Text.get_unit_size_with(None), 2);
        assert_eq!(
            Text.get_unit_size_with(Some(&crate::format::JetFormat::version_3())),
            1
        );
        assert_eq!(Text.to_unit_size(20, None), 10);
        assert_eq!(Text.from_unit_size(10, None), 20);
        assert_eq!(Long.to_unit_size(20, None), 20);
    }

    /// `fromByte` (and its `IOException` form).
    #[test]
    fn from_byte_matches_java() {
        assert_eq!(DataType::from_byte(0x01), Some(DataType::Boolean));
        assert_eq!(DataType::from_byte(0x14), Some(DataType::ExtDateTime));
        assert_eq!(DataType::from_byte(0xFE), None);
        assert_eq!(DataType::from_byte(0xFF), None);
        assert_eq!(
            DataType::try_from_byte(0xFF).unwrap_err().to_string(),
            "Unrecognized data type: -1"
        );
        assert_eq!(
            DataType::try_from_byte(0x00).unwrap_err().to_string(),
            "Unrecognized data type: 0"
        );
    }

    /// `getSQLType`'s `JackcessException` for the types without a JDBC type.
    #[test]
    fn sql_type_errors_match_java() {
        assert_eq!(
            DataType::Long.try_get_sql_type().unwrap(),
            sql_types::INTEGER
        );
        assert_eq!(
            DataType::Guid.try_get_sql_type().unwrap_err().to_string(),
            "Unsupported data type: GUID"
        );
    }

    /// The `fromSQLType` overloads: candidate selection by format and the
    /// alternate "long value" type.
    #[test]
    fn from_sql_type_matches_java() {
        use sql_types::*;
        assert_eq!(DataType::from_sql_type(BOOLEAN).unwrap(), DataType::Boolean);
        assert_eq!(DataType::from_sql_type(BIT).unwrap(), DataType::Byte);
        assert_eq!(DataType::from_sql_type(TINYINT).unwrap(), DataType::Byte);
        assert_eq!(DataType::from_sql_type(CLOB).unwrap(), DataType::Memo);
        assert_eq!(DataType::from_sql_type(BLOB).unwrap(), DataType::Ole);
        assert_eq!(DataType::from_sql_type(NVARCHAR).unwrap(), DataType::Text);

        // BIGINT's candidates are ordered (LONG, BIG_INT): without a format the
        // first one wins; V1997/V2003 do not support BIG_INT, V2016 does
        assert_eq!(DataType::from_sql_type(BIGINT).unwrap(), DataType::Long);
        assert_eq!(
            DataType::from_sql_type_with_format(BIGINT, 0, Some(FileFormat::V1997)).unwrap(),
            DataType::Long
        );
        assert_eq!(
            DataType::from_sql_type_with_format(BIGINT, 0, Some(FileFormat::V2003)).unwrap(),
            DataType::Long
        );
        assert_eq!(
            DataType::from_sql_type_with_format(BIGINT, 0, Some(FileFormat::V2016)).unwrap(),
            DataType::BigInt
        );

        // VARCHAR: the length is converted to bytes with the format's text unit
        assert_eq!(
            DataType::from_sql_type_with_length(VARCHAR, 0).unwrap(),
            DataType::Text
        );
        assert_eq!(
            DataType::from_sql_type_with_length(VARCHAR, 255).unwrap(),
            DataType::Text
        );
        assert_eq!(
            DataType::from_sql_type_with_length(VARCHAR, 256).unwrap(),
            DataType::Memo
        );
        // Jet3 has a one byte text unit, so 300 units still fit a TEXT column
        assert_eq!(
            DataType::from_sql_type_with_format(VARCHAR, 300, Some(FileFormat::V1997)).unwrap(),
            DataType::Text
        );
        assert_eq!(
            DataType::from_sql_type_with_format(VARCHAR, 300, Some(FileFormat::V2003)).unwrap(),
            DataType::Memo
        );
        // BINARY -> OLE alternate, VARBINARY -> OLE alternate
        assert_eq!(
            DataType::from_sql_type_with_length(BINARY, 300).unwrap(),
            DataType::Ole
        );
        assert_eq!(
            DataType::from_sql_type_with_length(VARBINARY, 300).unwrap(),
            DataType::Ole
        );
        assert_eq!(
            DataType::from_sql_type(12345).unwrap_err().to_string(),
            "Unsupported SQL type: 12345"
        );
    }
}
