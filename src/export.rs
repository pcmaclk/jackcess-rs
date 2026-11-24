//! Port of the data import/export utilities (`util.ExportUtil`,
//! `util.ImportUtil`, `util.ExportFilter`, `util.ImportFilter` and the simple
//! filter implementations) used to move rows between a database and delimited
//! text.
//!
//! The port follows the Java code closely, including its quirks:
//!
//! * the export delimiter is a literal string (the Java code builds a regular
//!   expression out of it, but only uses it for `find` on the raw value); a
//!   value is quoted when it contains the delimiter, the quote character or a
//!   carriage return / line feed, and embedded quote characters are doubled;
//! * a `null` value and an empty string both export as an empty field;
//! * binary (`byte[]`) values export as `ByteUtil.toHexString`'s *formatted*
//!   hex dump (upper case hex, a space between bytes, two spaces after every
//!   4 byte block and a `\n` after every 24 byte line);
//! * records are terminated with the platform line separator (`\r\n` on
//!   Windows), like `BufferedWriter.newLine()`;
//! * on import the delimiter is interpreted as a literal string.  The Java
//!   library documents it as a regular expression; the port accepts the
//!   common literal forms (including the `\t`, `\n`, `\r`, `\\` and
//!   `\Q...\E` escapes) and reports any other regular expression as
//!   unsupported instead of silently splitting differently;
//! * a quoted value may span lines, and the consumed line separators are
//!   replaced with the platform line separator (`ImportUtil.LINE_SEPARATOR`),
//!   exactly like the Java implementation.  The Java delimiter `Matcher` is
//!   bound to the first physical line and is not recreated after a wrap; that
//!   quirk is reproduced too (it can drop the fields following a multi line
//!   quoted value);
//! * a trailing delimiter does not produce an empty last token (it produces a
//!   `null` when the table has more columns), while an interior empty token is
//!   the empty string;
//! * a first line which is blank (only Java whitespace) makes the import a
//!   no-op which returns `None`;
//! * imported rows are written in batches of 200 (`COPY_TABLE_BATCH_SIZE`),
//!   which is observably different from a per row insert when the table has
//!   indexes.
//!
//! `ExportUtil.exportAll` is ported too ([`export_all`], [`export_all_ext`],
//! [`export_all_with`] and the non Java [`export_all_to_map`]): it exports
//! the names returned by [`Database::get_table_names`] (the normal and linked
//! tables, sorted like Java's `TreeSet<>(String.CASE_INSENSITIVE_ORDER)`, so
//! the system tables are *not* exported) to `<dir>/<table name>.<ext>`.
//! 5.0.4's `exportAll` does not create the output directory and does not
//! escape or replace any character of the table name: the names are combined
//! with the platform file system rules of `java.io.File`, so a table name
//! containing a path separator creates a sub path (and fails when the
//! subdirectory does not exist) and a name containing a character the
//! platform forbids fails at file creation.  Existing files are always
//! truncated (there is no "fail if exists" flag), and 5.0.4 has no append
//! flag and no `exportOutputStream`; streaming is [`export_table_to_writer`],
//! which appends when the caller supplies an appending writer, exactly like
//! Java's `exportWriter`.
//!
//! The text written for a numeric value is exactly Java's `Double.toString` /
//! `Float.toString` (`java_floating_to_string`): the shortest decimal which
//! round trips, laid out in Java's plain window (`10^-3 <= |v| < 10^7`) and in
//! computerized scientific notation outside it.  Two details of the JDK 19+
//! conversion are reproduced explicitly: a shortest significand of a single
//! digit is widened to two digits (rounded from the exact value, so
//! `Double.MIN_VALUE` prints `4.9E-324`, not `5.0E-324`), and an exact mid
//! point between two shortest candidates is resolved towards the even digit
//! (Rust's shortest formatting rounds such ties away from zero).
//!
//! Importing text into a `BINARY` / `OLE` (or unsupported variable/fixed
//! length) column writes the Java serialized form of the `String`, like
//! `ColumnImpl.toByteArray`'s `ObjectOutputStream.writeObject` fallback:
//! the `AC ED 00 05` stream header, a `TC_STRING` (or `TC_LONGSTRING` for a
//! modified UTF-8 form longer than 65535 bytes) and the Java "modified UTF-8"
//! bytes (``java_serialize_string``).  `byte[]` values are still stored as is.
//!
//! `BIG_BINARY` is a fixed length 4 byte *numeric reference* type in jackcess:
//! a text value is not serialized, it is parsed as a number
//! (`writeFixedLengthField` -> `toNumber(obj).intValue()`, and `toNumber`
//! parses a `String` with `Double.valueOf`).  The port does the same
//! conversion here ([`coerce_value`]) and reports Java's
//! `NumberFormatException` (`Error::NumberFormat`, with Java's exact
//! `getMessage()`) for text which is not a number.
//!
//! Not ported:
//!
//! * `ImportUtil.importResultSet` / `toColumns` (JDBC `ResultSet` import);
//! * importing a non text, non `byte[]` value into a binary column: leftovers
//!   from a custom [`ImportFilter`] are rejected instead of being java
//!   serialized as their Java object equivalent.
//!
//! 5.0.4 has no XML export utility; this module ports the delimited text
//! utilities which are the only ones in `com.healthmarketscience.jackcess.util`.

use std::io::Write;
use std::path::Path;

use crate::column::ColumnSpec;
use crate::database::Database;
use crate::datatype::DataType;
use crate::datenums::LocalDateTime;
use crate::error::{Error, Result};
use crate::tablecreator::TableSpec;
use crate::value::Value;

/// `ExportUtil.DEFAULT_DELIMITER`
pub const DEFAULT_DELIMITER: &str = ",";
/// `ExportUtil.DEFAULT_QUOTE_CHAR`
pub const DEFAULT_QUOTE_CHAR: char = '"';
/// `ExportUtil.DEFAULT_FILE_EXT`
pub const DEFAULT_FILE_EXT: &str = "csv";
/// `ImportUtil.COPY_TABLE_BATCH_SIZE`
pub const COPY_TABLE_BATCH_SIZE: usize = 200;

/// `System.lineSeparator()` / `ImportUtil.LINE_SEPARATOR`.
pub fn line_separator() -> &'static str {
    if cfg!(windows) {
        "\r\n"
    } else {
        "\n"
    }
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// The lightweight column descriptor handed to an [`ExportFilter`].
///
/// `index` is the position of the column in the table's column list (and thus
/// in the row array), so a filter may drop or reorder the exported columns
/// exactly like `ExportFilter.filterColumns` does with the Java `Column`
/// list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportColumn {
    pub index: usize,
    pub name: String,
    pub data_type: DataType,
}

/// Port of `ExportFilter`: allows customization of the exported columns and
/// rows.
pub trait ExportFilter {
    /// The columns that should be used to create the exported file.  The
    /// default keeps every column in table order.
    fn filter_columns(&self, columns: &[ExportColumn]) -> Vec<ExportColumn> {
        columns.to_vec()
    }

    /// The desired values for the row, in the (possibly filtered) column
    /// order.  Returning `None` skips the row.  The default keeps the row.
    fn filter_row(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
        Ok(Some(row.to_vec()))
    }
}

/// `SimpleExportFilter.INSTANCE`.
pub struct SimpleExportFilter;

impl ExportFilter for SimpleExportFilter {}

/// Port of `ExportUtil.Builder`'s options.
pub struct ExportOptions {
    /// the column delimiter (`ExportUtil.Builder.setDelimiter`), `","` by
    /// default
    pub delimiter: String,
    /// the quote character (`setQuote`), `'"'` by default
    pub quote: char,
    /// whether the first line contains the column names (`setHeader`)
    pub header: bool,
    /// the export filter (`setFilter`), [`SimpleExportFilter`] by default
    pub filter: Box<dyn ExportFilter>,
}

impl ExportOptions {
    pub fn new() -> ExportOptions {
        ExportOptions::default()
    }

    pub fn with_delimiter(mut self, delimiter: impl Into<String>) -> ExportOptions {
        self.delimiter = delimiter.into();
        self
    }

    pub fn with_quote(mut self, quote: char) -> ExportOptions {
        self.quote = quote;
        self
    }

    pub fn with_header(mut self, header: bool) -> ExportOptions {
        self.header = header;
        self
    }

    pub fn with_filter(mut self, filter: Box<dyn ExportFilter>) -> ExportOptions {
        self.filter = filter;
        self
    }
}

impl Default for ExportOptions {
    fn default() -> ExportOptions {
        ExportOptions {
            delimiter: DEFAULT_DELIMITER.to_string(),
            quote: DEFAULT_QUOTE_CHAR,
            header: false,
            filter: Box::new(SimpleExportFilter),
        }
    }
}

impl std::fmt::Debug for ExportOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExportOptions")
            .field("delimiter", &self.delimiter)
            .field("quote", &self.quote)
            .field("header", &self.header)
            .finish()
    }
}

/// Exports a table to a `String` (`ExportUtil.exportWriter`).
pub fn export_table(db: &mut Database, table_name: &str, opts: &ExportOptions) -> Result<String> {
    let mut out = Vec::new();
    export_table_to_writer(db, table_name, &mut out, opts)?;
    Ok(String::from_utf8(out)?)
}

/// Exports a table to a writer (`ExportUtil.exportWriter`).
pub fn export_table_to_writer<W: Write>(
    db: &mut Database,
    table_name: &str,
    out: &mut W,
    opts: &ExportOptions,
) -> Result<()> {
    let (page, columns) = {
        let table = db.get_table(table_name)?;
        (table.table_def_page_number, table.columns.clone())
    };
    let rows = db.read_table_rows(page)?;

    let all_columns: Vec<ExportColumn> = columns
        .iter()
        .enumerate()
        .map(|(index, col)| ExportColumn {
            index,
            name: col.name.clone(),
            data_type: col.data_type,
        })
        .collect();
    let filtered = opts.filter.filter_columns(&all_columns);

    // print the header row (if desired)
    if opts.header {
        for (i, column) in filtered.iter().enumerate() {
            write_csv_value(out, &column.name, opts.quote, &opts.delimiter)?;
            if i + 1 < filtered.len() {
                out.write_all(opts.delimiter.as_bytes())?;
            }
        }
        out.write_all(line_separator().as_bytes())?;
    }

    // print the data rows
    for row in rows.into_iter().flatten() {
        let mut unfiltered = Vec::with_capacity(filtered.len());
        for column in &filtered {
            unfiltered.push(row.get(column.index).cloned().unwrap_or(Value::Null));
        }

        let row_data = match opts.filter.filter_row(&unfiltered)? {
            Some(row) => row,
            None => continue,
        };
        if row_data.len() < filtered.len() {
            return Err(Error::IllegalState(format!(
                "export filter returned {} values for {} columns",
                row_data.len(),
                filtered.len()
            )));
        }

        for (i, value) in row_data.iter().take(filtered.len()).enumerate() {
            if let Some(text) = value_to_export_string(value)? {
                write_csv_value(out, &text, opts.quote, &opts.delimiter)?;
            }
            if i + 1 < filtered.len() {
                out.write_all(opts.delimiter.as_bytes())?;
            }
        }
        out.write_all(line_separator().as_bytes())?;
    }

    out.flush()?;
    Ok(())
}

/// Exports a table to a new file (`ExportUtil.exportFile`).  The file is
/// written as UTF-8 (the default charset of a modern JVM) and the file is
/// created or truncated (`Files.newBufferedWriter` defaults), like Java.
pub fn export_table_to_file(
    db: &mut Database,
    table_name: &str,
    path: &Path,
    opts: &ExportOptions,
) -> Result<()> {
    let file = std::fs::File::create(path)?;
    let mut out = std::io::BufWriter::new(file);
    export_table_to_writer(db, table_name, &mut out, opts)
}

/// The file name `ExportUtil.exportAll` uses for one table:
/// `<table name>.<ext>`.
///
/// Java builds this with `new File(dir, tableName + "." + ext)` and performs
/// no escaping or replacement at all, so the same is done here.  The result is
/// a plain string which is joined below `dir` with the platform's path rules
/// when it is written: a `/` (or `\` on Windows) inside a table name becomes a
/// path separator, and characters the platform forbids in a file name make
/// the eventual create fail.  A table name which differs from another only in
/// case cannot occur in an Access database (names are case insensitive), but
/// two names can still resolve to the same file on a case insensitive file
/// system or through path separators; like Java, the later export overwrites
/// the earlier one.
pub fn export_file_name(table_name: &str, ext: &str) -> String {
    format!("{table_name}.{ext}")
}

/// Exports every table into `dir` (`ExportUtil.exportAll(Database, File)`):
/// the default extension, no header, comma delimiter and double quote.
///
/// The output directory must already exist (Java does not create it either).
/// Files of existing tables are truncated.
pub fn export_all(db: &mut Database, dir: &Path) -> Result<()> {
    export_all_with(db, dir, DEFAULT_FILE_EXT, &ExportOptions::default())
}

/// Exports every table into `dir` with the given file extension
/// (`ExportUtil.exportAll(Database, File, String)`); the remaining options are
/// the defaults (no header, comma delimiter, double quote, no filter).
pub fn export_all_ext(db: &mut Database, dir: &Path, ext: &str) -> Result<()> {
    export_all_with(db, dir, ext, &ExportOptions::default())
}

/// Exports every table into `dir` with the given extension and options
/// (`ExportUtil.exportAll(Database, File, String, boolean, String, char,
/// ExportFilter)`).
///
/// The tables are exported in [`Database::get_table_names`] order, i.e. the
/// same case insensitive order Java's `TreeSet` reports.  The files are
/// `<table name>.<ext>` in `dir`; see [`export_file_name`] for the (lack of)
/// escaping and for collisions.  An error while writing one table stops the
/// export and leaves the files written so far in place, like the Java loop.
pub fn export_all_with(
    db: &mut Database,
    dir: &Path,
    ext: &str,
    opts: &ExportOptions,
) -> Result<()> {
    for table_name in db.get_table_names()? {
        let file_name = export_file_name(&table_name, ext);
        export_table_to_file(db, &table_name, &dir.join(file_name), opts)?;
    }
    Ok(())
}

/// Exports every table into an in memory map keyed by table name.
///
/// Java's `ExportUtil` only writes files; this streaming form is a port
/// convenience which produces exactly the bytes `exportAll` would write for
/// each table (same options and same table filtering, the system tables are
/// not included).  A map keeps the two tables whose names collide in a file
/// system (case insensitive names or path separators) apart.  The order of the
/// map is the map's own; use [`Database::get_table_names`] for the Java export
/// order.
pub fn export_all_to_map(
    db: &mut Database,
    opts: &ExportOptions,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut out = std::collections::BTreeMap::new();
    for table_name in db.get_table_names()? {
        out.insert(table_name.clone(), export_table(db, &table_name, opts)?);
    }
    Ok(out)
}

/// `ExportUtil.writeValue`: quotes the value only when the export delimiter,
/// the quote character or a newline character occurs in it, and doubles every
/// embedded quote character.
fn write_csv_value<W: Write>(out: &mut W, value: &str, quote: char, delimiter: &str) -> Result<()> {
    if !needs_quotes(value, delimiter, quote) {
        out.write_all(value.as_bytes())?;
        return Ok(());
    }

    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push(quote);
    for c in value.chars() {
        if c == quote {
            quoted.push(quote);
        }
        quoted.push(c);
    }
    quoted.push(quote);
    out.write_all(quoted.as_bytes())?;
    Ok(())
}

fn needs_quotes(value: &str, delimiter: &str, quote: char) -> bool {
    value.contains(delimiter)
        || value.contains(quote)
        || value.contains('\n')
        || value.contains('\r')
}

/// The text `ExportUtil` writes for a value: `String.valueOf` for everything
/// except `byte[]`, which is the formatted `ByteUtil.toHexString` dump.  A
/// `null` value is written as nothing at all.
fn value_to_export_string(value: &Value) -> Result<Option<String>> {
    match value {
        Value::Null => Ok(None),
        Value::Keep => Err(Error::IllegalState(
            "KEEP_VALUE cannot be exported as a value".to_string(),
        )),
        other => Ok(Some(java_value_to_string(other))),
    }
}

/// `String.valueOf` for a column value: the text Java's `toString` of the
/// matching column object produces (used by the export and as the
/// [`crate::columnfmt::ColumnFormatter`] fallback for values a format cannot
/// handle).  A `byte[]` is rendered as the formatted hex dump, like the
/// export does.
pub(crate) fn java_value_to_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bytes(bytes) => hex_dump(bytes),
        Value::Bool(b) => b.to_string(),
        Value::Byte(v) => (*v as i8).to_string(),
        Value::Int(v) => v.to_string(),
        Value::Long(v) => v.to_string(),
        Value::BigInt(v) => v.to_string(),
        Value::Float(v) => java_float_to_string(*v),
        Value::Double(v) => java_double_to_string(*v),
        Value::Decimal { unscaled, scale } => big_decimal_to_string(*unscaled, *scale),
        Value::Text(text) => text.clone(),
        Value::DateTime(ldt) => local_date_time_to_string(*ldt),
        Value::DateBits(bits) => {
            local_date_time_to_string(crate::column::ldt_from_local_date_double(*bits))
        }
        Value::Uuid(bytes) => guid_to_string(bytes),
        // `Column.KEEP_VALUE` is a String, so its Java toString is the text
        Value::Keep => "<KEEP_VALUE>".to_string(),
    }
}

/// `ByteUtil.toHexString(byte[])` (the `formatted = true` variant used by
/// `ExportUtil`): upper case hex bytes, a space between bytes, an extra space
/// after every 4 byte block and a line feed after every 24 byte line.
fn hex_dump(bytes: &[u8]) -> String {
    const BYTES_PER_BLOCK: usize = 4;
    const BYTES_PER_LINE: usize = 24;

    let mut out = String::with_capacity(bytes.len() * 3);
    let size = bytes.len();
    for (i, byte) in bytes.iter().enumerate() {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0F));
        let next = i + 1;
        if next < size {
            if next % BYTES_PER_LINE == 0 {
                out.push('\n');
            } else {
                out.push(' ');
                if next % BYTES_PER_BLOCK == 0 {
                    out.push(' ');
                }
            }
        }
    }
    out
}

fn hex_digit(value: u8) -> char {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    DIGITS[(value & 0x0F) as usize] as char
}

/// `Worker` style formatting of a GUID: the read value is the raw, little
/// endian byte order of the stored value, so the first three components are
/// byte swapped and the result is braced and upper case
/// (`ColumnImpl.readGUIDValue`).
fn guid_to_string(raw: &[u8; 16]) -> String {
    let mut bytes = *raw;
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    format!(
        "{{{}-{}-{}-{}-{}}}",
        hex_of(&bytes[0..4]),
        hex_of(&bytes[4..6]),
        hex_of(&bytes[6..8]),
        hex_of(&bytes[8..10]),
        hex_of(&bytes[10..16])
    )
}

fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(hex_digit(byte >> 4));
        out.push(hex_digit(byte & 0x0F));
    }
    out
}

/// `java.time.LocalDateTime.toString()`: `yyyy-MM-ddTHH:mm`, with the seconds
/// and then the fraction of a second only when they are non zero.  The
/// fraction is printed with 3, 6 or 9 digits (trailing zero groups removed),
/// exactly like `LocalTime.toString`.
fn local_date_time_to_string(ldt: LocalDateTime) -> String {
    // The port's `ldt_from_local_date_double` returns a nanosecond count which
    // may have rounded up to a whole second (Java's `Duration.ofSeconds`
    // normalizes that carry into the seconds); normalize it here too so the
    // displayed value matches the Java `LocalDateTime`.
    let ldt = normalize_local_date_time(ldt);
    let mut out = String::with_capacity(19);
    out.push_str(&local_date_to_string(ldt.year, ldt.month, ldt.day));
    out.push('T');
    out.push_str(&format!("{:02}:{:02}", ldt.hour, ldt.minute));
    if ldt.second > 0 || ldt.nano > 0 {
        out.push_str(&format!(":{:02}", ldt.second));
        if ldt.nano > 0 {
            out.push('.');
            if ldt.nano.is_multiple_of(1_000_000) {
                out.push_str(&format!("{:03}", ldt.nano / 1_000_000));
            } else if ldt.nano.is_multiple_of(1_000) {
                out.push_str(&format!("{:06}", ldt.nano / 1_000));
            } else {
                out.push_str(&format!("{:09}", ldt.nano));
            }
        }
    }
    out
}

/// A `LocalDateTime` whose nanosecond field holds a full second or more is
/// carried into the time (the representation Java's `LocalDateTime` can never
/// have).
fn normalize_local_date_time(ldt: LocalDateTime) -> LocalDateTime {
    if ldt.nano < 1_000_000_000 {
        return ldt;
    }
    let extra_seconds = (ldt.nano / 1_000_000_000) as i64;
    let nano = ldt.nano % 1_000_000_000;
    let mut seconds = ldt.second_of_day() + extra_seconds;
    let mut days = ldt.to_epoch_day();
    days += seconds.div_euclid(86_400);
    seconds = seconds.rem_euclid(86_400);
    let mut out = LocalDateTime::from_epoch_day(days);
    out.hour = (seconds / 3600) as u32;
    out.minute = ((seconds % 3600) / 60) as u32;
    out.second = (seconds % 60) as u32;
    out.nano = nano;
    out
}

/// `java.time.LocalDate.toString()`.
fn local_date_to_string(year: i32, month: u32, day: u32) -> String {
    let mut out = String::with_capacity(10);
    if (0..10_000).contains(&year) {
        out.push_str(&format!("{:04}", year));
    } else if (-10_000..0).contains(&year) {
        out.push('-');
        out.push_str(&format!("{:04}", -year));
    } else if year >= 10_000 {
        out.push('+');
        out.push_str(&year.to_string());
    } else {
        out.push_str(&year.to_string());
    }
    out.push_str(&format!("-{:02}-{:02}", month, day));
    out
}

/// `Double.toString`: the shortest decimal representation which round trips,
/// laid out with a fractional part for plain values and in computerized
/// scientific notation outside `[1e-3, 1e7)`.
///
/// The JDK 19+ conversion is not quite "shortest": its shortest significand is
/// never a single digit in a context which Java prints with a fractional part,
/// and an exact mid point between two digits is rounded to even.  Both rules
/// are applied here on top of Rust's shortest round trip formatting.
pub fn java_double_to_string(value: f64) -> String {
    java_floating_to_string(value, false)
}

/// `Float.toString`.
pub fn java_float_to_string(value: f32) -> String {
    java_floating_to_string(value as f64, true)
}

fn java_floating_to_string(value: f64, is_float: bool) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .to_string();
    }

    // `{:e}` gives the shortest round trip mantissa with a single leading
    // digit ("1.2345e-7", "1e2", ...), the digits Java's conversion starts
    // from (Rust rounds a tie away from zero, Java rounds it to even).
    let (mut digits, mut exponent) = shortest_digits(value, is_float);
    if digits.len() > 1 {
        round_shortest_tie(&mut digits, exponent, value, is_float);
    } else if !(-3..0).contains(&exponent) {
        // A single digit significand is always widened to two digits when a
        // fractional digit is required (everything but the `0.00d` plain
        // form): the JDK rounds the extra digit from the exact value, so
        // `Double.MIN_VALUE` becomes `4.9E-324` and `1e23` becomes `1.0E23`.
        let (wide, wide_exponent) = exact_two_digit_rounding(value, is_float);
        digits = wide;
        exponent = wide_exponent;
    }

    let negative = value.is_sign_negative();
    let mut out = String::with_capacity(24);
    if negative {
        out.push('-');
    }

    // Java's plain notation window is 10^-3 <= |value| < 10^7, i.e. a leading
    // digit exponent from -3 through 6.
    if (-3..7).contains(&exponent) {
        if exponent >= 0 {
            let int_len = exponent as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.push_str(&"0".repeat(int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exponent - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push(digits.as_bytes()[0] as char);
        out.push('.');
        if digits.len() > 1 {
            out.push_str(&digits[1..]);
        } else {
            out.push('0');
        }
        out.push('E');
        out.push_str(&exponent.to_string());
    }
    out
}

/// The shortest round trip digits of `value` (as `Float` when `is_float`) and
/// the exponent of the leading digit (`10^exponent <= |value| < 10^(exponent+1)`).
fn shortest_digits(value: f64, is_float: bool) -> (String, i32) {
    let scientific = if is_float {
        format!("{:e}", value as f32)
    } else {
        format!("{:e}", value)
    };
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("Rust's exponential float format always contains an exponent");
    let digits = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let exponent = exponent.parse().expect("exponent is an integer");
    (digits, exponent)
}

/// The correctly rounded two significant digit decimal of `value` (as `Float`
/// when `is_float`), and the exponent of its leading digit.
fn exact_two_digit_rounding(value: f64, is_float: bool) -> (String, i32) {
    let scientific = if is_float {
        format!("{:.1e}", value as f32)
    } else {
        format!("{:.1e}", value)
    };
    let (mantissa, exponent) = scientific
        .split_once('e')
        .expect("Rust's exponential float format always contains an exponent");
    let digits = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let exponent = exponent.parse().expect("exponent is an integer");
    (digits, exponent)
}

/// Rounds the shortest significand to even when the exact value is a mid point
/// between the shortest candidate and its neighbour.
///
/// A mid point means the exact decimal expansion of `value` has exactly one
/// more significant digit than the shortest form, and that digit is a `5`
/// (`value = (digits + 0.5) * 10^k`).  Writing `|value| = m * 2^e` with `m`
/// odd, the number of significant decimal digits of the exact expansion is
/// `exponent + (-e) + 1`, so the mid point case is simply `-e == len - exponent`.
/// Rust rounds the tie away from zero (it picks the larger last digit); Java
/// picks the even one, i.e. the digit one lower when the last digit is odd.
fn round_shortest_tie(digits: &mut String, exponent: i32, value: f64, is_float: bool) {
    let (mut mantissa, mut binary_exponent) = if is_float {
        let bits = (value as f32).abs().to_bits();
        let biased = ((bits >> 23) & 0xFF) as i32;
        let fraction = (bits & 0x7F_FFFF) as u64;
        if biased == 0 {
            (fraction, -149)
        } else {
            (fraction | 0x80_0000, biased - 127 - 23)
        }
    } else {
        let bits = value.abs().to_bits();
        let biased = ((bits >> 52) & 0x7FF) as i32;
        let fraction = bits & 0xF_FFFF_FFFF_FFFF;
        if biased == 0 {
            (fraction, -1074)
        } else {
            (fraction | 0x10_0000_0000_0000, biased - 1023 - 52)
        }
    };
    if mantissa == 0 {
        return;
    }
    while mantissa & 1 == 0 {
        mantissa >>= 1;
        binary_exponent += 1;
    }
    if binary_exponent >= 0 {
        // an integer exact value is never a mid point of its shortest form
        return;
    }
    if -binary_exponent != digits.len() as i32 - exponent {
        return;
    }
    if let Some(last) = digits.pop() {
        let digit = last.to_digit(10).expect("a decimal digit");
        let rounded = if digit % 2 == 1 { digit - 1 } else { digit };
        digits.push(std::char::from_digit(rounded, 10).expect("a decimal digit"));
    }
}

/// `ColumnImpl.toByteArray`'s fallback for a `String`: the object's Java
/// serialized form (`ObjectOutputStream.writeObject`).
///
/// A `String` is written as the `AC ED 00 05` stream header followed by
/// `TC_STRING` (`0x74`, a two byte length) or, when the modified UTF-8 form is
/// longer than 65535 bytes, `TC_LONGSTRING` (`0x7C`, an eight byte length) and
/// the Java "modified UTF-8" encoding of the characters: CESU-8 for the UTF-16
/// code units (so a supplementary character takes six bytes) and `C0 80` for
/// U+0000.
fn java_serialize_string(text: &str) -> Vec<u8> {
    let mut utf: Vec<u8> = Vec::with_capacity(text.len() + 8);
    for unit in text.encode_utf16() {
        match unit {
            0 => {
                utf.push(0xC0);
                utf.push(0x80);
            }
            0x0001..=0x007F => utf.push(unit as u8),
            0x0080..=0x07FF => {
                utf.push(0xC0 | (unit >> 6) as u8);
                utf.push(0x80 | (unit & 0x3F) as u8);
            }
            _ => {
                utf.push(0xE0 | (unit >> 12) as u8);
                utf.push(0x80 | ((unit >> 6) & 0x3F) as u8);
                utf.push(0x80 | (unit & 0x3F) as u8);
            }
        }
    }

    let mut out = Vec::with_capacity(utf.len() + 12);
    out.extend_from_slice(&[0xAC, 0xED, 0x00, 0x05]);
    if utf.len() <= u16::MAX as usize {
        out.push(0x74);
        out.extend_from_slice(&(utf.len() as u16).to_be_bytes());
    } else {
        out.push(0x7C);
        out.extend_from_slice(&(utf.len() as u64).to_be_bytes());
    }
    out.extend_from_slice(&utf);
    out
}

/// `BigDecimal.toString` for a value built from an unscaled integer and a
/// scale (the representation jackcess' MONEY and NUMERIC columns use).
fn big_decimal_to_string(unscaled: i128, scale: i32) -> String {
    if scale == 0 {
        return unscaled.to_string();
    }

    let negative = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let precision = digits.len() as i64;
    // BigDecimal.layoutChars: plain notation when the scale is non negative
    // and the adjusted exponent (precision - scale - 1) is at least -6.
    let adjusted = precision - scale as i64 - 1;
    let mut out = String::new();
    if negative {
        out.push('-');
    }

    if scale > 0 && adjusted >= -6 {
        if digits.len() > scale as usize {
            let split = digits.len() - scale as usize;
            out.push_str(&digits[..split]);
            out.push('.');
            out.push_str(&digits[split..]);
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat(scale as usize - digits.len()));
            out.push_str(&digits);
        }
    } else {
        // computerized scientific notation: `BigDecimal.layoutChars` prints
        // the first coefficient digit, a '.' and the remaining digits only
        // when there are any, 'E' and the adjusted exponent (with a '+' for a
        // non negative one).
        out.push(digits.as_bytes()[0] as char);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('E');
        if adjusted >= 0 {
            out.push('+');
        }
        out.push_str(&adjusted.to_string());
    }
    out
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

/// The type information used when [`ImportOptions::column_types`] creates a
/// table from a header line (the equivalent of returning modified
/// `ColumnBuilder`s from a Java `ImportFilter.filterColumns`).  The column
/// name always comes from the header line, like `ImportUtil` uses the header
/// tokens as column names.
#[derive(Clone, Debug)]
pub struct ImportColumn {
    pub data_type: DataType,
    /// `ColumnBuilder.setLength` (bytes)
    pub length: Option<i16>,
    /// `ColumnBuilder.setPrecision`
    pub precision: Option<u8>,
    /// `ColumnBuilder.setScale`
    pub scale: Option<u8>,
    /// `ColumnBuilder.setCompressedUnicode`
    pub compressed_unicode: bool,
}

impl ImportColumn {
    pub fn new(data_type: DataType) -> ImportColumn {
        ImportColumn {
            data_type,
            length: None,
            precision: None,
            scale: None,
            compressed_unicode: false,
        }
    }

    /// The column type `ImportUtil` gives a header column: `TEXT` with the
    /// maximum (255 byte) length.
    pub fn text() -> ImportColumn {
        ImportColumn::new(DataType::Text).with_length(255)
    }

    pub fn with_length(mut self, length: i16) -> ImportColumn {
        self.length = Some(length);
        self
    }

    pub fn with_precision(mut self, precision: u8) -> ImportColumn {
        self.precision = Some(precision);
        self
    }

    pub fn with_scale(mut self, scale: u8) -> ImportColumn {
        self.scale = Some(scale);
        self
    }

    pub fn with_compressed_unicode(mut self, compressed: bool) -> ImportColumn {
        self.compressed_unicode = compressed;
        self
    }

    fn to_spec(&self, name: &str) -> ColumnSpec {
        let mut spec = ColumnSpec::new(name, self.data_type);
        if let Some(length) = self.length {
            spec = spec.with_length(length);
        }
        if let Some(precision) = self.precision {
            spec = spec.with_precision(precision);
        }
        if let Some(scale) = self.scale {
            spec = spec.with_scale(scale);
        }
        if self.compressed_unicode {
            spec = spec.with_compressed_unicode(true);
        }
        spec
    }
}

/// Port of `ImportFilter`: allows customization of the columns used to
/// create the imported table and of the imported rows.
pub trait ImportFilter {
    /// The columns that should be used to create the imported table.  Called
    /// only when the import creates a new table, with the header names
    /// already parsed (and escaped) and, unless
    /// [`ImportOptions::column_types`] was given, every type set to
    /// `TEXT(255)`.
    fn filter_columns(&self, _columns: &mut Vec<ImportColumn>) -> Result<()> {
        Ok(())
    }

    /// The desired values for the row, before the values are converted to the
    /// target columns' types.  Returning `None` skips the row.
    fn filter_row(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
        Ok(Some(row.to_vec()))
    }
}

/// `SimpleImportFilter.INSTANCE`.
pub struct SimpleImportFilter;

impl ImportFilter for SimpleImportFilter {}

/// Port of `ImportUtil.Builder`'s options.
pub struct ImportOptions {
    /// the name of the new (or existing) table (`setTableName`)
    pub table_name: String,
    /// the delimiter (`setDelimiter`), `","` by default.  Interpreted as a
    /// literal string; see [`delimiter_literal`] for the accepted escapes.
    pub delimiter: String,
    /// the quote character (`setQuote`), `'"'` by default
    pub quote: char,
    /// whether the first line is a header row (`setHeader`), `true` by
    /// default
    pub header: bool,
    /// use the existing table with the given name if there is one
    /// (`setUseExistingTable`), `false` by default
    pub use_existing_table: bool,
    /// the explicit column types used when a new table is created.  `None`
    /// creates every column as `TEXT(255)`, like `ImportUtil`.
    pub column_types: Option<Vec<ImportColumn>>,
    /// the import filter (`setFilter`), [`SimpleImportFilter`] by default
    pub filter: Box<dyn ImportFilter>,
}

impl ImportOptions {
    pub fn new(table_name: impl Into<String>) -> ImportOptions {
        ImportOptions {
            table_name: table_name.into(),
            delimiter: DEFAULT_DELIMITER.to_string(),
            quote: DEFAULT_QUOTE_CHAR,
            header: true,
            use_existing_table: false,
            column_types: None,
            filter: Box::new(SimpleImportFilter),
        }
    }

    pub fn with_delimiter(mut self, delimiter: impl Into<String>) -> ImportOptions {
        self.delimiter = delimiter.into();
        self
    }

    pub fn with_quote(mut self, quote: char) -> ImportOptions {
        self.quote = quote;
        self
    }

    pub fn with_header(mut self, header: bool) -> ImportOptions {
        self.header = header;
        self
    }

    pub fn with_use_existing_table(mut self, use_existing: bool) -> ImportOptions {
        self.use_existing_table = use_existing;
        self
    }

    pub fn with_column_types(mut self, column_types: Vec<ImportColumn>) -> ImportOptions {
        self.column_types = Some(column_types);
        self
    }

    pub fn with_filter(mut self, filter: Box<dyn ImportFilter>) -> ImportOptions {
        self.filter = filter;
        self
    }
}

impl std::fmt::Debug for ImportOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportOptions")
            .field("table_name", &self.table_name)
            .field("delimiter", &self.delimiter)
            .field("quote", &self.quote)
            .field("header", &self.header)
            .field("use_existing_table", &self.use_existing_table)
            .field("column_types", &self.column_types)
            .finish()
    }
}

/// Imports delimited text into a database (`ImportUtil.importReader`).
///
/// Returns the name of the imported table (which may have a numeric suffix
/// when a table with the requested name already exists and
/// `use_existing_table` is not set), or `None` when the first line is blank
/// (the Java method returns `null` in that case).
pub fn import_text(text: &str, db: &mut Database, opts: &ImportOptions) -> Result<Option<String>> {
    let delimiter: Vec<char> = delimiter_literal(&opts.delimiter)?.chars().collect();
    let mut reader = LineReader::new(text);

    let first_line = match reader.read_line() {
        Some(line) => line,
        None => return Ok(None),
    };
    if is_blank(&first_line) {
        return Ok(None);
    }

    let mut table_name = escape_identifier(&opts.table_name);
    let mut header = opts.header;

    let exists = db.find_table(&table_name)?.is_some();
    if !opts.use_existing_table || !exists {
        let header_tokens = split_line(&first_line, &delimiter, opts.quote, &mut reader, 0)?;

        let mut names = Vec::with_capacity(header_tokens.len());
        for token in &header_tokens {
            match token {
                Value::Text(name) => {
                    let name = escape_identifier(name);
                    if name.trim().is_empty() {
                        return Err(Error::IllegalArgument(
                            "column name must have non-blank name".to_string(),
                        ));
                    }
                    names.push(name);
                }
                _ => {
                    return Err(Error::IllegalArgument(
                        "missing column name in header line".to_string(),
                    ))
                }
            }
        }

        let mut types = match &opts.column_types {
            Some(types) => {
                if types.len() != names.len() {
                    return Err(Error::IllegalArgument(format!(
                        "{} column types given for {} header columns",
                        types.len(),
                        names.len()
                    )));
                }
                types.clone()
            }
            None => vec![ImportColumn::text(); names.len()],
        };
        opts.filter.filter_columns(&mut types)?;
        if types.len() != names.len() {
            return Err(Error::IllegalArgument(
                "import filter changed the number of columns".to_string(),
            ));
        }

        let specs: Vec<ColumnSpec> = names
            .iter()
            .zip(types.iter())
            .map(|(name, ty)| ty.to_spec(name))
            .collect();
        table_name = unique_table_name(db, &table_name)?;
        db.create_table(&TableSpec::new(table_name.clone(), specs))?;

        // the first row was a header row
        header = true;
    }

    let columns = db.get_table(&table_name)?.columns.clone();
    let num_columns = columns.len();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(COPY_TABLE_BATCH_SIZE);

    if !header {
        // first line is _not_ a header line
        let data = split_line(
            &first_line,
            &delimiter,
            opts.quote,
            &mut reader,
            num_columns,
        )?;
        // a value conversion failure is a row write failure: `TableImpl.addRows`
        // reports it as a `BatchUpdateException` (see [`import_batch_failure`])
        match prepare_row(&columns, data, opts.filter.as_ref()) {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(error) => {
                return Err(import_batch_failure(db, &table_name, &rows, error));
            }
        }
    }

    while let Some(line) = reader.read_line() {
        let data = split_line(&line, &delimiter, opts.quote, &mut reader, num_columns)?;
        match prepare_row(&columns, data, opts.filter.as_ref()) {
            Ok(Some(row)) => {
                rows.push(row);
                if rows.len() == COPY_TABLE_BATCH_SIZE {
                    db.add_rows(&table_name, &rows)?;
                    rows.clear();
                }
            }
            Ok(None) => {}
            Err(error) => {
                return Err(import_batch_failure(db, &table_name, &rows, error));
            }
        }
    }
    if !rows.is_empty() {
        db.add_rows(&table_name, &rows)?;
    }

    Ok(Some(table_name))
}

/// The error `TableImpl.addRows` throws for a batch whose row write failed.
///
/// `TableImpl.addRows` writes the rows of a batch one at a time; when one row
/// fails it flushes the rows which were already written, updates the table
/// definition and throws
/// `BatchUpdateException(updateCount, "Failed adding rows (Db=...;Table=...)",
/// cause)`, whose message is `msg + ": " + cause` and whose `updateCount` is
/// the number of rows written before the failing one.  `ImportUtil` always
/// imports in batches, so a value which cannot be written surfaces this way
/// (the port converts the values while preparing the batch, hence the explicit
/// wrapper around the failure).
fn import_batch_failure(
    db: &mut Database,
    table_name: &str,
    pending: &[Vec<Value>],
    cause: Error,
) -> Error {
    let update_count = pending.len() as i32;
    // flush the rows written before the failing one
    let flush = if update_count > 0 {
        db.add_rows(table_name, pending)
    } else {
        Ok(())
    };
    let context = match db.get_table(table_name) {
        Ok(table) => format!(
            "Failed adding rows (Db={};Table={})",
            table.db_name, table.name
        ),
        Err(_) => "Failed adding rows".to_string(),
    };
    match flush {
        Ok(()) => Error::batch_update(update_count, context, cause),
        // a failing flush means the status of the batch is unknown: Java clears
        // the update count and reports the flush failure as the cause
        Err(flush_error) => Error::batch_update(0, context, flush_error),
    }
}

/// Imports a delimited text file (`ImportUtil.importFile`).  The file is read
/// as UTF-8.
pub fn import_file(path: &Path, db: &mut Database, opts: &ImportOptions) -> Result<Option<String>> {
    let text = std::fs::read_to_string(path)?;
    import_text(&text, db, opts)
}

/// Applies the import filter to a parsed row and converts its values to the
/// target column types, the way `Table.addRows` converts the values it is
/// given.
fn prepare_row(
    columns: &[crate::column::Column],
    data: Vec<Value>,
    filter: &dyn ImportFilter,
) -> Result<Option<Vec<Value>>> {
    let mut row = match filter.filter_row(&data)? {
        Some(row) => row,
        None => return Ok(None),
    };
    // `TableImpl.addRows` accepts a short row (it is padded with nulls) and
    // ignores values past the last column.
    row.resize(columns.len(), Value::Null);

    let mut converted = Vec::with_capacity(columns.len());
    for (column, value) in columns.iter().zip(row.iter()) {
        converted.push(coerce_value(column, value)?);
    }
    Ok(Some(converted))
}

/// Converts one parsed value to the stored representation of the target
/// column, mirroring what the Java library does while writing a row
/// (`ColumnImpl.writeFixedLengthField` / `writeRealData`), including its
/// errors: e.g. a date/time column cannot be filled from text (the Java
/// library needs a `LocalDateTime`/`Date`; use an [`ImportFilter`]).
fn coerce_value(column: &crate::column::Column, value: &Value) -> Result<Value> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let data_type = column.data_type;
    match data_type {
        DataType::Boolean => Ok(Value::Bool(match value {
            Value::Bool(b) => *b,
            Value::Text(text) => text.eq_ignore_ascii_case("true"),
            other => match other.as_f64() {
                Some(v) => v != 0.0,
                None => {
                    return Err(Error::InvalidValue(format!(
                        "cannot convert {:?} to BOOLEAN",
                        value
                    )))
                }
            },
        })),
        DataType::Byte => {
            // `Double.byteValue()` is `(byte)(int)doubleValue()`.  The value is
            // returned as a LONG: `Column::write_fixed_length_field` converts a
            // `Value::Byte` through `to_number`, which reads its u8 as an
            // unsigned number, so a java style negative byte (e.g. -1 stored as
            // 255) would saturate to 127 on the way back out.
            let d = numeric_value(value)?;
            Ok(Value::Long((d as i32) as i8 as i32))
        }
        DataType::Int => {
            let d = numeric_value(value)?;
            Ok(Value::Int((d as i32) as i16))
        }
        DataType::Long => Ok(Value::Long(numeric_value(value)? as i32)),
        DataType::BigInt => Ok(Value::BigInt(numeric_value(value)? as i64)),
        DataType::Float => Ok(Value::Float(numeric_value(value)? as f32)),
        DataType::Double => Ok(Value::Double(numeric_value(value)?)),
        DataType::Money => {
            let (unscaled, scale) = decimal_value(value, 4)?;
            Ok(Value::Decimal { unscaled, scale })
        }
        DataType::Numeric => {
            let target_scale = column.scale as i32;
            let (unscaled, scale) = decimal_value(value, target_scale)?;
            let precision = decimal_precision(unscaled);
            if precision > column.precision as usize {
                return Err(Error::InvalidValue(format!(
                    "Numeric value is too big for specified precision {}: {}",
                    column.precision,
                    big_decimal_to_string(unscaled, scale)
                )));
            }
            Ok(Value::Decimal { unscaled, scale })
        }
        DataType::ShortDateTime | DataType::ExtDateTime => match value {
            Value::DateTime(_) | Value::DateBits(_) => Ok(value.clone()),
            _ => Err(Error::Unsupported(format!(
                "a {} column cannot be imported from text (the Java library accepts only \
                 date/time values there); convert the value in an ImportFilter",
                data_type.get_name()
            ))),
        },
        DataType::Guid => match value {
            Value::Text(text) => Ok(Value::Text(text.clone())),
            Value::Uuid(bytes) => Ok(Value::Uuid(*bytes)),
            _ => Ok(Value::Text(java_string_value(value)?)),
        },
        DataType::Text | DataType::Memo => Ok(Value::Text(java_string_value(value)?)),
        DataType::Binary
        | DataType::Ole
        | DataType::UnsupportedVarLen
        | DataType::UnsupportedFixedLen => match value {
            Value::Bytes(bytes) => Ok(Value::Bytes(bytes.clone())),
            // `ColumnImpl.toByteArray`: anything which is neither a byte[]
            // nor a stream is written through an `ObjectOutputStream`, which
            // for the `String` values an import produces is the string's Java
            // serialized form.
            Value::Text(text) => Ok(Value::Bytes(java_serialize_string(text))),
            other => Err(Error::Unsupported(format!(
                "a {} column can only be imported from text or byte[] values; the Java \
                 library java-serializes {:?} here, which this port does not reproduce",
                data_type.get_name(),
                other
            ))),
        },
        // `BIG_BINARY` is a fixed length 4 byte *numeric reference* type: Java
        // does not serialize a text value for it, it converts it with
        // `writeFixedLengthField` -> `toNumber(obj).intValue()`, and `toNumber`
        // parses a `String` with `Double.valueOf` (so the value is
        // `(int)doubleValue()`: NaN is 0 and out of range values saturate).
        // Text is therefore accepted, and a text which is not a number is
        // rejected with Java's `NumberFormatException`.
        DataType::BigBinary => match value {
            Value::Text(text) => {
                let d = parse_java_double(text)?;
                Ok(Value::Long(d as i32))
            }
            // `booleanToInteger`: Access style, true is -1 and false is 0
            Value::Bool(value) => Ok(Value::Long(if *value { -1 } else { 0 })),
            // numbers go through `toNumber` unchanged
            Value::Byte(_)
            | Value::Int(_)
            | Value::Long(_)
            | Value::BigInt(_)
            | Value::Float(_)
            | Value::Double(_)
            | Value::Decimal { .. } => Ok(value.clone()),
            // `Double.valueOf(value.toString())`: the `toString` of a `byte[]`
            // (or of the GUID bytes) embeds an identity hash code, so Java's
            // message cannot be reproduced; the value is rejected like Java
            // rejects it
            Value::Bytes(_) | Value::Uuid(_) => Err(Error::NumberFormat(
                "For input string: \"[B@<identity hash code>\"".to_string(),
            )),
            // a date value would be converted to its date double by Java's
            // `toNumber`; an import never produces one (the text import has no
            // date columns), so it stays unsupported
            other => Err(Error::Unsupported(format!(
                "a BIG_BINARY column cannot be imported from {:?} (the Java library \
                 converts the value with toNumber)",
                other
            ))),
        },
        DataType::ComplexType => Ok(value.clone()),
    }
}

/// `ColumnImpl.toNumber`: text is parsed as a `Double`, `null` is 0 and
/// booleans are -1 (true) / 0 (false).
fn numeric_value(value: &Value) -> Result<f64> {
    match value {
        Value::Null => Ok(0.0),
        Value::Bool(b) => Ok(if *b { -1.0 } else { 0.0 }),
        Value::Byte(v) => Ok(*v as i8 as f64),
        Value::Int(v) => Ok(*v as f64),
        Value::Long(v) => Ok(*v as f64),
        Value::BigInt(v) => Ok(*v as f64),
        Value::Float(v) => Ok(*v as f64),
        Value::Double(v) => Ok(*v),
        Value::Decimal { unscaled, scale } => Ok(*unscaled as f64 / 10f64.powi(*scale)),
        Value::Text(text) => parse_java_double(text),
        _ => Err(Error::InvalidValue(format!(
            "cannot convert {:?} to a number",
            value
        ))),
    }
}

/// `Double.valueOf(String)`: Java's `NumberFormatException` message for a text
/// which is not a number.
///
/// `FloatingDecimal.readJavaFormatString` trims the leading/trailing
/// whitespace, rejects an empty value with `"empty String"` and reports every
/// other failure with `For input string: "<the original text>"`.
fn number_format_exception(text: &str) -> Error {
    if java_trim(text).is_empty() {
        Error::NumberFormat("empty String".to_string())
    } else {
        Error::NumberFormat(format!("For input string: \"{}\"", text))
    }
}

/// `Double.valueOf(String)`.
///
/// The input is trimmed like Java (`String.trim`, i.e. every character up to
/// U+0020), the `NaN`/`Infinity` spellings are recognized exactly (Java is case
/// sensitive and accepts them without a type suffix), an optional `d`/`f` type
/// suffix is stripped from a numeric form, and the numeric form is validated
/// against Java's grammar before Rust parses it: Rust's `f64` parser accepts
/// `inf`/`infinity`/`nan` in any case and other spellings Java rejects.
/// Hexadecimal floating point literals (`0x1.8p1`, a Java extension) are
/// parsed here as well.
fn parse_java_double(text: &str) -> Result<f64> {
    let s = java_trim(text);
    match s {
        "NaN" | "+NaN" | "-NaN" => return Ok(f64::NAN),
        "Infinity" | "+Infinity" => return Ok(f64::INFINITY),
        "-Infinity" => return Ok(f64::NEG_INFINITY),
        _ => {}
    }
    // `FloatTypeSuffix`, only valid after a numeric form
    let numeric = match s.chars().last() {
        Some(last @ ('d' | 'D' | 'f' | 'F')) if s.len() > 1 => &s[..s.len() - last.len_utf8()],
        _ => s,
    };
    if numeric.is_empty() {
        return Err(number_format_exception(text));
    }
    if let Some(value) = parse_java_hex_double(numeric) {
        return value.map_err(|_| number_format_exception(text));
    }
    if !is_java_decimal_double(numeric) {
        return Err(number_format_exception(text));
    }
    numeric
        .parse::<f64>()
        .map_err(|_| number_format_exception(text))
}

/// Java's decimal floating point grammar (without the sign, which is part of
/// it, and without the type suffix, which [`parse_java_double`] strips):
/// `Sign? (Digits ('.' Digits?)? | '.' Digits) ExponentPart?`.
fn is_java_decimal_double(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    let mut digits = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
        digits += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
            digits += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let exponent_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exponent_start {
            return false;
        }
    }
    i == bytes.len()
}

/// `Some(Ok(value))` for a hexadecimal floating point literal, `Some(Err(()))`
/// for a malformed one (which starts like a hex literal) and `None` for a text
/// which is not a hex literal at all.
///
/// Java's grammar is `Sign? 0x HexDigits ('.' HexDigits?)? | 0x '.' HexDigits`
/// followed by a *required* binary exponent (`[pP] Sign? Digits`).  The
/// significand is accumulated in an `f64`, which is exact up to 53 bits; a
/// longer literal can round differently from Java's correctly rounded parser.
fn parse_java_hex_double(s: &str) -> Option<std::result::Result<f64, ()>> {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut negative = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        negative = bytes[i] == b'-';
        i += 1;
    }
    if !s[i..].starts_with("0x") && !s[i..].starts_with("0X") {
        return None;
    }
    i += 2;
    let mut significand = 0f64;
    let mut digits = 0usize;
    let mut exponent: i32 = 0;
    while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
        significand = significand * 16.0 + f64::from(hex_digit_value(bytes[i]));
        i += 1;
        digits += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
            significand = significand * 16.0 + f64::from(hex_digit_value(bytes[i]));
            i += 1;
            digits += 1;
            exponent -= 4;
        }
    }
    if digits == 0 {
        return Some(Err(()));
    }
    if i >= bytes.len() || (bytes[i] != b'p' && bytes[i] != b'P') {
        return Some(Err(()));
    }
    i += 1;
    let mut exponent_negative = false;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        exponent_negative = bytes[i] == b'-';
        i += 1;
    }
    let exponent_start = i;
    let mut decimal_exponent: i32 = 0;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        // saturate: the result is 0 or infinite long before this matters
        decimal_exponent = decimal_exponent
            .saturating_mul(10)
            .saturating_add(i32::from(bytes[i] - b'0'));
        i += 1;
    }
    if i == exponent_start || i != bytes.len() {
        return Some(Err(()));
    }
    if exponent_negative {
        decimal_exponent = -decimal_exponent;
    }
    let exponent = exponent.saturating_add(decimal_exponent);
    let value = significand * 2f64.powi(exponent);
    Some(Ok(if negative { -value } else { value }))
}

fn hex_digit_value(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => byte - b'A' + 10,
    }
}

/// `String.trim()`: removes every character up to and including U+0020.
fn java_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c <= '\u{20}')
}

/// `ColumnImpl.toBigDecimal` for the values an import can produce, rescaled to
/// the column's scale with `BigDecimal.setScale`'s exactness requirement (it
/// throws when digits would have to be rounded away).
fn decimal_value(value: &Value, target_scale: i32) -> Result<(i128, i32)> {
    let (unscaled, scale) = match value {
        Value::Text(text) => parse_big_decimal(text)?,
        Value::Byte(v) => (*v as i8 as i128, 0),
        Value::Int(v) => (*v as i128, 0),
        Value::Long(v) => (*v as i128, 0),
        Value::BigInt(v) => (*v as i128, 0),
        Value::Bool(b) => (if *b { -1 } else { 0 }, 0),
        Value::Decimal { unscaled, scale } => (*unscaled, *scale),
        _ => {
            return Err(Error::Unsupported(format!(
                "cannot convert {:?} to a scaled decimal (the Java library converts numbers \
                 through their binary double representation)",
                value
            )))
        }
    };
    rescale_decimal(unscaled, scale, target_scale)
}

fn rescale_decimal(unscaled: i128, scale: i32, target: i32) -> Result<(i128, i32)> {
    if scale == target {
        return Ok((unscaled, target));
    }
    if scale < target {
        let factor = pow10_i128(target - scale)?;
        let scaled = unscaled
            .checked_mul(factor)
            .ok_or_else(|| Error::InvalidValue("decimal value is out of range".to_string()))?;
        return Ok((scaled, target));
    }
    let factor = pow10_i128(scale - target)?;
    if unscaled % factor != 0 {
        return Err(Error::InvalidValue(format!(
            "value {} has too many decimal places for scale {}",
            big_decimal_to_string(unscaled, scale),
            target
        )));
    }
    Ok((unscaled / factor, target))
}

fn pow10_i128(power: i32) -> Result<i128> {
    let mut result: i128 = 1;
    for _ in 0..power {
        result = result
            .checked_mul(10)
            .ok_or_else(|| Error::InvalidValue("decimal value is out of range".to_string()))?;
    }
    Ok(result)
}

fn decimal_precision(unscaled: i128) -> usize {
    unscaled.unsigned_abs().to_string().len()
}

/// `new BigDecimal(String)`: optional sign, decimal digits with an optional
/// decimal point and an optional exponent.  No whitespace is allowed.
fn parse_big_decimal(text: &str) -> Result<(i128, i32)> {
    let mut chars = text.chars().peekable();
    let mut negative = false;
    match chars.peek() {
        Some('+') => {
            chars.next();
        }
        Some('-') => {
            negative = true;
            chars.next();
        }
        _ => {}
    }

    let mut digits = String::new();
    let mut fraction_len: i32 = 0;
    let mut seen_dot = false;
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            digits.push(c);
            if seen_dot {
                fraction_len += 1;
            }
            chars.next();
        } else if c == '.' && !seen_dot {
            seen_dot = true;
            chars.next();
        } else {
            break;
        }
    }
    if digits.is_empty() {
        return Err(Error::InvalidValue(format!("invalid decimal '{text}'")));
    }

    let mut exponent: i32 = 0;
    if let Some(&c) = chars.peek() {
        if c == 'e' || c == 'E' {
            chars.next();
            let mut exp_negative = false;
            match chars.peek() {
                Some('+') => {
                    chars.next();
                }
                Some('-') => {
                    exp_negative = true;
                    chars.next();
                }
                _ => {}
            }
            let mut exp_digits = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() {
                    exp_digits.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            if exp_digits.is_empty() {
                return Err(Error::InvalidValue(format!("invalid decimal '{text}'")));
            }
            exponent = exp_digits
                .parse::<i32>()
                .map_err(|_| Error::InvalidValue(format!("invalid decimal '{text}'")))?;
            if exp_negative {
                exponent = -exponent;
            }
        }
    }
    if chars.next().is_some() {
        return Err(Error::InvalidValue(format!("invalid decimal '{text}'")));
    }

    let mut unscaled: i128 = digits
        .parse::<i128>()
        .map_err(|_| Error::InvalidValue(format!("decimal value is out of range: '{text}'")))?;
    if negative {
        unscaled = -unscaled;
    }
    // BigDecimal's scale is `fraction digits - exponent`; a negative scale is
    // possible and is normalized by rescale_decimal.
    Ok((unscaled, fraction_len - exponent))
}

/// The literal string which a Java import delimiter regex matches, for the
/// subset of regular expressions the port supports.  Throws
/// [`Error::Unsupported`] for a real regular expression so a delimiter never
/// silently splits differently than the Java library would.
pub fn delimiter_literal(delimiter: &str) -> Result<String> {
    if delimiter.is_empty() {
        return Err(Error::Unsupported(
            "an empty delimiter would loop forever in ImportUtil.splitLine".to_string(),
        ));
    }
    let mut out = String::with_capacity(delimiter.len());
    let mut chars = delimiter.chars().peekable();
    let mut quoted = false;
    while let Some(c) = chars.next() {
        if quoted {
            if c == '\\' && chars.peek() == Some(&'E') {
                chars.next();
                quoted = false;
            } else {
                out.push(c);
            }
            continue;
        }
        match c {
            '\\' => {
                let escaped = chars.next().ok_or_else(|| {
                    Error::Unsupported(format!(
                        "delimiter '{delimiter}' ends with a lone backslash"
                    ))
                })?;
                match escaped {
                    't' => out.push('\t'),
                    'n' => out.push('\n'),
                    'r' => out.push('\r'),
                    'f' => out.push('\u{0C}'),
                    'a' => out.push('\u{07}'),
                    'e' => out.push('\u{1B}'),
                    'Q' => quoted = true,
                    other if !other.is_alphanumeric() => out.push(other),
                    other => {
                        return Err(Error::Unsupported(format!(
                            "delimiter '{delimiter}' uses the regular expression escape \
                             '\\{other}'; the Rust port supports literal delimiters only"
                        )))
                    }
                }
            }
            '.' | '*' | '+' | '?' | '|' | '(' | ')' | '[' | '^' | '$' => {
                return Err(Error::Unsupported(format!(
                    "delimiter '{delimiter}' is a regular expression; the Rust port supports \
                     literal delimiters only"
                )))
            }
            other => out.push(other),
        }
    }
    if quoted {
        // Java's `\Q` quotes up to `\E` or the end of the pattern
        return Ok(out);
    }
    Ok(out)
}

/// `BufferedReader.readLine()` over a string: `\r\n`, `\n` and `\r` all end a
/// line and the terminator is not returned.  `None` means end of input.
struct LineReader {
    chars: Vec<char>,
    pos: usize,
}

impl LineReader {
    fn new(text: &str) -> LineReader {
        LineReader {
            chars: text.chars().collect(),
            pos: 0,
        }
    }

    fn read_line(&mut self) -> Option<String> {
        if self.pos >= self.chars.len() {
            return None;
        }
        let start = self.pos;
        while self.pos < self.chars.len() {
            match self.chars[self.pos] {
                '\n' => {
                    let line: String = self.chars[start..self.pos].iter().collect();
                    self.pos += 1;
                    return Some(line);
                }
                '\r' => {
                    let line: String = self.chars[start..self.pos].iter().collect();
                    self.pos += 1;
                    if self.pos < self.chars.len() && self.chars[self.pos] == '\n' {
                        self.pos += 1;
                    }
                    return Some(line);
                }
                _ => self.pos += 1,
            }
        }
        Some(self.chars[start..self.pos].iter().collect())
    }
}

/// `ImportUtil.splitLine`: splits a line using the literal `delimiter` and the
/// quote character, reading more lines while a quoted value spans newlines.
/// The result is padded with `Value::Null` up to `num_columns` entries.
///
/// The Java method creates one delimiter `Matcher` for the first physical line
/// and never recreates it when a quoted value spans newlines, even though the
/// `line` local is reassigned to the continuation line.  Every delimiter
/// search therefore still runs against the *first* line while the returned
/// tokens are substrings of the *current* line.  That quirk is reproduced
/// here because it changes which fields are imported after a multi line
/// quoted value (typically the fields after the closing quote on the last
/// continuation line are dropped).
fn split_line(
    line: &str,
    delimiter: &[char],
    quote: char,
    reader: &mut LineReader,
    num_columns: usize,
) -> Result<Vec<Value>> {
    // the Java `Matcher` is bound to the first physical line of this call
    let matcher_chars: Vec<char> = line.chars().collect();
    let mut line_chars: Vec<char> = matcher_chars.clone();
    let mut tokens: Vec<String> = Vec::new();
    let mut sb = String::new();
    let mut idx = 0usize;

    while idx < line_chars.len() {
        if line_chars[idx] == quote {
            // find quoted value
            sb.clear();
            idx += 1;
            loop {
                match find_char(&line_chars, quote, idx) {
                    Some(end_idx) => {
                        sb.extend(line_chars[idx..end_idx].iter());
                        idx = end_idx + 1;
                        if idx < line_chars.len() && line_chars[idx] == quote {
                            // embedded quote
                            sb.push(quote);
                            idx += 1;
                        } else {
                            break;
                        }
                    }
                    None => {
                        // line wrap
                        sb.extend(line_chars[idx..].iter());
                        sb.push_str(line_separator());
                        idx = 0;
                        match reader.read_line() {
                            Some(next) => {
                                line_chars = next.chars().collect();
                            }
                            None => {
                                return Err(Error::Jackcess(format!(
                                    "Missing end of quoted value {sb}"
                                )))
                            }
                        }
                    }
                }
            }

            tokens.push(sb.clone());

            // skip next delim (searched in the first physical line, like the
            // stale Java Matcher)
            idx = match find_slice(&matcher_chars, delimiter, idx) {
                Some(pos) => pos + delimiter.len(),
                None => line_chars.len(),
            };
        } else if let Some(pos) = find_slice(&matcher_chars, delimiter, idx) {
            // next unquoted value: the end index comes from the first physical
            // line, the token from the current one (exactly like Java)
            if pos > line_chars.len() {
                return Err(Error::Jackcess(format!(
                    "split position {} is past the continuation line length {}",
                    pos,
                    line_chars.len()
                )));
            }
            tokens.push(line_chars[idx..pos].iter().collect());
            idx = pos + delimiter.len();
        } else {
            // trailing token
            tokens.push(line_chars[idx..].iter().collect());
            idx = line_chars.len();
        }
    }

    let count = tokens.len().max(num_columns);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        match tokens.get(i) {
            Some(token) => out.push(Value::Text(token.clone())),
            None => out.push(Value::Null),
        }
    }
    Ok(out)
}

fn find_char(haystack: &[char], needle: char, from: usize) -> Option<usize> {
    haystack
        .iter()
        .enumerate()
        .skip(from)
        .find(|(_, c)| **c == needle)
        .map(|(i, _)| i)
}

fn find_slice(haystack: &[char], needle: &[char], from: usize) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| haystack[i..i + needle.len()] == *needle)
}

/// `Character.isWhitespace` (which differs from Rust's `char::is_whitespace`
/// for the non breaking space characters).
fn is_java_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{0B}'
            | '\u{0C}'
            | '\r'
            | '\u{1C}'..='\u{1F}'
            | ' '
            | '\u{1680}'
            | '\u{2000}'..='\u{2006}'
            | '\u{2008}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{205F}'
            | '\u{3000}'
    )
}

/// `StringUtil.isBlank`.
fn is_blank(text: &str) -> bool {
    text.chars().all(is_java_whitespace)
}

/// The text Java's `String.valueOf` produces for an arbitrary object (used
/// when a filter puts a non textual value in a text column).
fn java_string_value(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok(String::new()),
        Value::Text(text) => Ok(text.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Byte(v) => Ok((*v as i8).to_string()),
        Value::Int(v) => Ok(v.to_string()),
        Value::Long(v) => Ok(v.to_string()),
        Value::BigInt(v) => Ok(v.to_string()),
        Value::Float(v) => Ok(java_float_to_string(*v)),
        Value::Double(v) => Ok(java_double_to_string(*v)),
        Value::Decimal { unscaled, scale } => Ok(big_decimal_to_string(*unscaled, *scale)),
        Value::DateTime(ldt) => Ok(local_date_time_to_string(*ldt)),
        Value::DateBits(bits) => Ok(local_date_time_to_string(
            crate::column::ldt_from_local_date_double(*bits),
        )),
        Value::Uuid(bytes) => Ok(uuid_string(bytes)),
        Value::Bytes(_) => Err(Error::Unsupported(
            "the Java library's byte[].toString() identity hash cannot be reproduced".to_string(),
        )),
        Value::Keep => Err(Error::IllegalState(
            "KEEP_VALUE cannot be imported as a value".to_string(),
        )),
    }
}

/// `java.util.UUID.toString()` for a raw (stored, little endian) GUID value.
fn uuid_string(raw: &[u8; 16]) -> String {
    let mut bytes = *raw;
    bytes[0..4].reverse();
    bytes[4..6].reverse();
    bytes[6..8].reverse();
    format!(
        "{}-{}-{}-{}-{}",
        hex_of(&bytes[0..4]).to_ascii_lowercase(),
        hex_of(&bytes[4..6]).to_ascii_lowercase(),
        hex_of(&bytes[6..8]).to_ascii_lowercase(),
        hex_of(&bytes[8..10]).to_ascii_lowercase(),
        hex_of(&bytes[10..16]).to_ascii_lowercase()
    )
}

/// `TableBuilder.escapeIdentifier`: reserved words get an `x` prefix.
fn escape_identifier(name: &str) -> String {
    if is_reserved_word(name) {
        format!("x{name}")
    } else {
        name.to_string()
    }
}

fn is_reserved_word(name: &str) -> bool {
    RESERVED_WORDS.contains(&name.to_lowercase().as_str())
}

/// `TableBuilder.ReservedWords.VALUES`.
static RESERVED_WORDS: &[&str] = &[
    "add",
    "all",
    "alphanumeric",
    "alter",
    "and",
    "any",
    "application",
    "as",
    "asc",
    "assistant",
    "autoincrement",
    "avg",
    "between",
    "binary",
    "bit",
    "boolean",
    "by",
    "byte",
    "char",
    "character",
    "column",
    "compactdatabase",
    "constraint",
    "container",
    "count",
    "counter",
    "create",
    "createdatabase",
    "createfield",
    "creategroup",
    "createindex",
    "createobject",
    "createproperty",
    "createrelation",
    "createtabledef",
    "createuser",
    "createworkspace",
    "currency",
    "currentuser",
    "database",
    "date",
    "datetime",
    "delete",
    "desc",
    "description",
    "disallow",
    "distinct",
    "distinctrow",
    "document",
    "double",
    "drop",
    "echo",
    "else",
    "end",
    "eqv",
    "error",
    "exists",
    "exit",
    "false",
    "field",
    "fields",
    "fillcache",
    "float",
    "float4",
    "float8",
    "foreign",
    "form",
    "forms",
    "from",
    "full",
    "function",
    "general",
    "getobject",
    "getoption",
    "gotopage",
    "group",
    "group by",
    "guid",
    "having",
    "idle",
    "ieeedouble",
    "ieeesingle",
    "if",
    "ignore",
    "imp",
    "in",
    "index",
    "indexes",
    "inner",
    "insert",
    "inserttext",
    "int",
    "integer",
    "integer1",
    "integer2",
    "integer4",
    "into",
    "is",
    "join",
    "key",
    "lastmodified",
    "left",
    "level",
    "like",
    "logical",
    "logical1",
    "long",
    "longbinary",
    "longtext",
    "macro",
    "match",
    "max",
    "min",
    "mod",
    "memo",
    "module",
    "money",
    "move",
    "name",
    "newpassword",
    "no",
    "not",
    "null",
    "number",
    "numeric",
    "object",
    "oleobject",
    "off",
    "on",
    "openrecordset",
    "option",
    "or",
    "order",
    "outer",
    "owneraccess",
    "parameter",
    "parameters",
    "partial",
    "percent",
    "pivot",
    "primary",
    "procedure",
    "property",
    "queries",
    "query",
    "quit",
    "real",
    "recalc",
    "recordset",
    "references",
    "refresh",
    "refreshlink",
    "registerdatabase",
    "relation",
    "repaint",
    "repairdatabase",
    "report",
    "reports",
    "requery",
    "right",
    "screen",
    "section",
    "select",
    "set",
    "setfocus",
    "setoption",
    "short",
    "single",
    "smallint",
    "some",
    "sql",
    "stdev",
    "stdevp",
    "string",
    "sum",
    "table",
    "tabledef",
    "tabledefs",
    "tableid",
    "text",
    "time",
    "timestamp",
    "top",
    "transform",
    "true",
    "type",
    "union",
    "unique",
    "update",
    "user",
    "value",
    "values",
    "var",
    "varp",
    "varbinary",
    "varchar",
    "where",
    "with",
    "workspace",
    "xor",
    "year",
    "yes",
    "yesno",
];

/// `ImportUtil.createUniqueTable`: appends 2, 3, ... to the base name until
/// the name is not taken.
fn unique_table_name(db: &mut Database, base_name: &str) -> Result<String> {
    if db.find_table(base_name)?.is_none() {
        return Ok(base_name.to_string());
    }
    let mut counter = 2;
    loop {
        let name = format!("{base_name}{counter}");
        counter += 1;
        if db.find_table(&name)?.is_none() {
            return Ok(name);
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::format::FileFormat;
    use crate::tablecreator::TableSpec;
    use crate::value::Value;

    /// Test scratch directory inside the workspace (the sandbox does not allow
    /// writes outside it for spawned test processes).
    fn test_tmp_dir() -> std::path::PathBuf {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    fn split(text: &str, delim: &str, quote: char, num_columns: usize) -> Vec<Value> {
        let delimiter: Vec<char> = delimiter_literal(delim).unwrap().chars().collect();
        let mut reader = LineReader::new(text);
        let line = reader.read_line().unwrap();
        split_line(&line, &delimiter, quote, &mut reader, num_columns).unwrap()
    }

    fn texts(values: &[Value]) -> Vec<Option<String>> {
        values
            .iter()
            .map(|v| match v {
                Value::Text(s) => Some(s.clone()),
                Value::Null => None,
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    }

    #[test]
    fn csv_value_quoting_matches_java() {
        let terms = |value: &str| {
            let mut out = Vec::new();
            write_csv_value(&mut out, value, '"', ",").unwrap();
            String::from_utf8(out).unwrap()
        };
        assert_eq!(terms("plain"), "plain");
        assert_eq!(terms("comma,here"), "\"comma,here\"");
        assert_eq!(terms("quote\"here"), "\"quote\"\"here\"");
        assert_eq!(terms("line\nfeed"), "\"line\nfeed\"");
        assert_eq!(terms("carriage\rreturn"), "\"carriage\rreturn\"");
        assert_eq!(terms(""), "");
        assert_eq!(terms(" spaces "), " spaces ");
        // the delimiter is matched literally, so a semicolon is not special
        let mut out = Vec::new();
        write_csv_value(&mut out, "semi;colon", '"', ";").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\"semi;colon\"");
        // a quote character other than '"'
        let mut out = Vec::new();
        write_csv_value(&mut out, "it's", '\'', ",").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "'it''s'");
    }

    #[test]
    fn hex_dump_matches_byte_util() {
        assert_eq!(hex_dump(&[]), "");
        assert_eq!(hex_dump(&[0x00, 0x0F, 0xFF]), "00 0F FF");
        // two spaces after every 4 byte block
        assert_eq!(hex_dump(&[1, 2, 3, 4, 5]), "01 02 03 04  05");
        // a newline after every 24 byte line
        let bytes: Vec<u8> = (0..26).collect();
        let dump = hex_dump(&bytes);
        assert_eq!(dump.matches('\n').count(), 1);
        let lines: Vec<&str> = dump.split('\n').collect();
        // 23 single separators plus the extra space after bytes 4, 8, 12, 16
        // and 20 (byte 24 ends the line with a newline instead)
        assert_eq!(lines[0].matches(' ').count(), 28);
        assert!(lines[0].starts_with("00 01 02 03  04"));
        assert_eq!(lines[1], "18 19");
    }

    #[test]
    fn double_and_float_formatting_matches_java() {
        assert_eq!(java_double_to_string(0.0), "0.0");
        assert_eq!(java_double_to_string(-0.0), "-0.0");
        assert_eq!(java_double_to_string(1.0), "1.0");
        assert_eq!(java_double_to_string(1.5), "1.5");
        assert_eq!(java_double_to_string(-2.25), "-2.25");
        assert_eq!(java_double_to_string(0.001), "0.001");
        assert_eq!(java_double_to_string(0.0001), "1.0E-4");
        assert_eq!(java_double_to_string(1.0e7), "1.0E7");
        assert_eq!(java_double_to_string(1234567.0), "1234567.0");
        assert_eq!(java_double_to_string(12345678.0), "1.2345678E7");
        assert_eq!(java_double_to_string(1.0e300), "1.0E300");
        assert_eq!(java_double_to_string(f64::NAN), "NaN");
        assert_eq!(java_double_to_string(f64::INFINITY), "Infinity");
        assert_eq!(java_double_to_string(f64::NEG_INFINITY), "-Infinity");
        // the decimal-short values keep Java's plain and scientific forms
        assert_eq!(java_double_to_string(0.1), "0.1");
        assert_eq!(java_double_to_string(0.29), "0.29");
        assert_eq!(java_double_to_string(2.0), "2.0");
        assert_eq!(java_double_to_string(100.0), "100.0");
        assert_eq!(java_double_to_string(1.0e22), "1.0E22");
        assert_eq!(java_double_to_string(1.0e23), "1.0E23");
        // subnormals: Java's conversion widens the single digit significands
        assert_eq!(java_double_to_string(f64::from_bits(1)), "4.9E-324");
        assert_eq!(java_double_to_string(f64::from_bits(2)), "9.9E-324");
        assert_eq!(java_double_to_string(f64::from_bits(3)), "1.5E-323");
        assert_eq!(java_double_to_string(f64::from_bits(0x0A)), "4.9E-323");
        assert_eq!(java_double_to_string(f64::from_bits(0x0C)), "5.9E-323");
        assert_eq!(java_double_to_string(f64::from_bits(0x0E)), "6.9E-323");
        assert_eq!(java_double_to_string(f64::from_bits(0x10)), "7.9E-323");
        assert_eq!(
            java_double_to_string(f64::MIN_POSITIVE),
            "2.2250738585072014E-308"
        );
        assert_eq!(java_double_to_string(1.0e-308), "1.0E-308");
        assert_eq!(java_double_to_string(1.0e-323), "9.9E-324");
        assert_eq!(java_double_to_string(f64::MAX), "1.7976931348623157E308");
        assert_eq!(java_double_to_string(-f64::MAX), "-1.7976931348623157E308");
        // an exact mid point between two shortest candidates is rounded to
        // even (Rust would round it away from zero)
        assert_eq!(
            java_double_to_string(2_251_799_813_685_247.8),
            "2.2517998136852478E15"
        );
        assert_eq!(
            java_double_to_string(-2_251_799_813_685_247.8),
            "-2.2517998136852478E15"
        );

        assert_eq!(java_float_to_string(1.0), "1.0");
        assert_eq!(java_float_to_string(1.25), "1.25");
        assert_eq!(java_float_to_string(0.001), "0.001");
        assert_eq!(java_float_to_string(1.0e7), "1.0E7");
        assert_eq!(java_float_to_string(f32::NAN), "NaN");
        assert_eq!(java_float_to_string(f32::from_bits(1)), "1.4E-45");
        assert_eq!(java_float_to_string(f32::from_bits(2)), "2.8E-45");
        assert_eq!(java_float_to_string(f32::from_bits(4)), "5.6E-45");
        assert_eq!(java_float_to_string(f32::from_bits(7)), "9.8E-45");
        assert_eq!(java_float_to_string(f32::MIN_POSITIVE), "1.1754944E-38");
        assert_eq!(java_float_to_string(f32::MAX), "3.4028235E38");
        // a float mid point (3052641.25f) is rounded to even as well
        assert_eq!(java_float_to_string(3_052_641.3), "3052641.2");
        assert_eq!(java_float_to_string(-3_052_641.3), "-3052641.2");
    }

    #[test]
    fn java_string_serialization_matches_object_output_stream() {
        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|b| format!("{b:02X}")).collect()
        }
        // ObjectOutputStream: stream header + TC_STRING + modified UTF-8
        assert_eq!(
            hex(&java_serialize_string("hello")),
            "ACED000574000568656C6C6F"
        );
        assert_eq!(hex(&java_serialize_string("")), "ACED0005740000");
        // U+0000 is the two byte C0 80 form
        assert_eq!(
            hex(&java_serialize_string("a\u{0}b")),
            "ACED000574000461C08062"
        );
        assert_eq!(hex(&java_serialize_string("\u{0}")), "ACED0005740002C080");
        // a BMP character outside Latin-1 is three bytes
        assert_eq!(hex(&java_serialize_string("\u{e9}")), "ACED0005740002C3A9");
        assert_eq!(
            hex(&java_serialize_string("\u{ffff}")),
            "ACED0005740003EFBFBF"
        );
        // a supplementary character is its surrogate pair, six bytes (CESU-8)
        assert_eq!(
            hex(&java_serialize_string("\u{1F600}")),
            "ACED0005740006EDA0BDEDB880"
        );
        // a modified UTF-8 form longer than 65535 bytes uses TC_LONGSTRING
        let long = "a".repeat(70_000);
        let bytes = java_serialize_string(&long);
        assert_eq!(&bytes[..5], &[0xAC, 0xED, 0x00, 0x05, 0x7C]);
        assert_eq!(&bytes[5..13], &[0, 0, 0, 0, 0, 0x01, 0x11, 0x70]);
        assert_eq!(bytes.len(), 13 + 70_000);
    }

    #[test]
    fn export_value_formatting_of_numbers() {
        assert_eq!(
            value_to_export_string(&Value::Byte(200)).unwrap(),
            Some("-56".to_string())
        );
        assert_eq!(
            value_to_export_string(&Value::Byte(0)).unwrap(),
            Some("0".to_string())
        );
        assert_eq!(
            value_to_export_string(&Value::Float(2.5)).unwrap(),
            Some("2.5".to_string())
        );
        assert_eq!(
            value_to_export_string(&Value::Float(1.0e7)).unwrap(),
            Some("1.0E7".to_string())
        );
        assert_eq!(
            value_to_export_string(&Value::Double(1234567.0)).unwrap(),
            Some("1234567.0".to_string())
        );
        assert_eq!(
            value_to_export_string(&Value::Decimal {
                unscaled: 123400,
                scale: 4
            })
            .unwrap(),
            Some("12.3400".to_string())
        );
        assert_eq!(
            value_to_export_string(&Value::Bool(true)).unwrap(),
            Some("true".to_string())
        );
        assert_eq!(value_to_export_string(&Value::Null).unwrap(), None);
    }

    #[test]
    fn big_decimal_formatting_matches_java() {
        assert_eq!(big_decimal_to_string(123, 0), "123");
        assert_eq!(big_decimal_to_string(123, 2), "1.23");
        assert_eq!(big_decimal_to_string(-1, 4), "-0.0001");
        assert_eq!(big_decimal_to_string(0, 4), "0.0000");
        assert_eq!(big_decimal_to_string(123400, 4), "12.3400");
        assert_eq!(big_decimal_to_string(1, 10), "1E-10");
        assert_eq!(big_decimal_to_string(123, 10), "1.23E-8");
        assert_eq!(big_decimal_to_string(1, -3), "1E+3");
        assert_eq!(big_decimal_to_string(123, -5), "1.23E+7");
    }

    #[test]
    fn local_date_time_formatting_matches_java() {
        assert_eq!(
            local_date_time_to_string(LocalDateTime::new(2020, 3, 15, 0, 0, 0, 0)),
            "2020-03-15T00:00"
        );
        assert_eq!(
            local_date_time_to_string(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0)),
            "2020-03-15T12:30:45"
        );
        assert_eq!(
            local_date_time_to_string(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 123_000_000)),
            "2020-03-15T12:30:45.123"
        );
        assert_eq!(
            local_date_time_to_string(LocalDateTime::new(2020, 3, 15, 12, 30, 0, 1_000)),
            "2020-03-15T12:30:00.000001"
        );
        assert_eq!(
            local_date_time_to_string(LocalDateTime::new(2020, 3, 15, 12, 30, 0, 1)),
            "2020-03-15T12:30:00.000000001"
        );
        // a nanosecond count which rounded up to a whole second is carried
        // into the time, like Java's Duration.ofSeconds normalization
        assert_eq!(
            local_date_time_to_string(LocalDateTime::new(2022, 6, 7, 8, 9, 9, 1_000_000_000)),
            "2022-06-07T08:09:10"
        );
        assert_eq!(
            local_date_time_to_string(LocalDateTime::new(2022, 12, 31, 23, 59, 59, 1_000_000_000)),
            "2023-01-01T00:00"
        );
        assert_eq!(local_date_to_string(1, 2, 3), "0001-02-03");
        assert_eq!(local_date_to_string(-1, 2, 3), "-0001-02-03");
        assert_eq!(local_date_to_string(10000, 2, 3), "+10000-02-03");
        assert_eq!(local_date_to_string(-10000, 2, 3), "-10000-02-03");
    }

    #[test]
    fn guid_formatting_matches_java() {
        // stored little endian bytes of {01234567-89AB-CDEF-0123-456789ABCDEF}
        let raw = [
            0x67, 0x45, 0x23, 0x01, 0xAB, 0x89, 0xEF, 0xCD, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB,
            0xCD, 0xEF,
        ];
        assert_eq!(
            guid_to_string(&raw),
            "{01234567-89AB-CDEF-0123-456789ABCDEF}"
        );
        assert_eq!(uuid_string(&raw), "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn import_delimiter_literals() {
        assert_eq!(delimiter_literal(",").unwrap(), ",");
        assert_eq!(delimiter_literal(";").unwrap(), ";");
        assert_eq!(delimiter_literal("\\t").unwrap(), "\t");
        assert_eq!(delimiter_literal("\t").unwrap(), "\t");
        assert_eq!(delimiter_literal("\\|").unwrap(), "|");
        assert_eq!(delimiter_literal("\\Q.|\\E").unwrap(), ".|");
        assert_eq!(delimiter_literal("\\Q.|").unwrap(), ".|");
        assert!(delimiter_literal("|").is_err());
        assert!(delimiter_literal("\\d+").is_err());
        assert!(delimiter_literal("").is_err());
    }

    #[test]
    fn split_line_matches_import_util() {
        assert_eq!(
            texts(&split("a,b,c", ",", '"', 0)),
            vec![Some("a".into()), Some("b".into()), Some("c".into())]
        );
        // an interior empty field is the empty string
        assert_eq!(
            texts(&split("a,,c", ",", '"', 0)),
            vec![Some("a".into()), Some("".into()), Some("c".into())]
        );
        // a trailing delimiter does not produce a token; the table's column
        // count pads with null
        assert_eq!(
            texts(&split("a,b,", ",", '"', 0)),
            vec![Some("a".into()), Some("b".into())]
        );
        assert_eq!(
            texts(&split("a,b,", ",", '"', 3)),
            vec![Some("a".into()), Some("b".into()), None]
        );
        // quoted values keep delimiters, quotes and newlines
        assert_eq!(
            texts(&split("\"a,b\",c", ",", '"', 0)),
            vec![Some("a,b".into()), Some("c".into())]
        );
        assert_eq!(
            texts(&split("\"a\"\"b\"", ",", '"', 0)),
            vec![Some("a\"b".into())]
        );
        assert_eq!(texts(&split("\"\"", ",", '"', 0)), vec![Some("".into())]);
        // after a quoted value the parser skips to the next delimiter
        assert_eq!(
            texts(&split("\"a\"x,b", ",", '"', 0)),
            vec![Some("a".into()), Some("b".into())]
        );
        // an unquoted quote is just text
        assert_eq!(
            texts(&split("a\"b,c", ",", '"', 0)),
            vec![Some("a\"b".into()), Some("c".into())]
        );
    }

    #[test]
    fn split_line_spans_newlines() {
        let delimiter: Vec<char> = delimiter_literal(",").unwrap().chars().collect();
        // the delimiter Matcher stays bound to the first physical line, so the
        // field after the closing quote of the continuation line is not
        // imported (ImportUtil.splitLine's stale Matcher quirk)
        let mut reader = LineReader::new("\"line1\nline2\",c\nnext");
        let line = reader.read_line().unwrap();
        let values = split_line(&line, &delimiter, '"', &mut reader, 0).unwrap();
        assert_eq!(
            texts(&values),
            vec![Some(format!("line1{}line2", line_separator()))]
        );
        assert_eq!(reader.read_line().as_deref(), Some("next"));

        // a quoted value which is the last field of the continuation line
        // keeps every earlier token
        let mut reader = LineReader::new("a,\"line1\nline2\"\nnext");
        let line = reader.read_line().unwrap();
        let values = split_line(&line, &delimiter, '"', &mut reader, 0).unwrap();
        assert_eq!(
            texts(&values),
            vec![
                Some("a".into()),
                Some(format!("line1{}line2", line_separator()))
            ]
        );
        assert_eq!(reader.read_line().as_deref(), Some("next"));
    }

    #[test]
    fn split_line_multiline_value_drops_following_fields_like_java() {
        // `ExportGen`'s second fixture row: the first physical line is long
        // enough that the stale matcher finds its comma inside "comma, value"
        let delimiter: Vec<char> = delimiter_literal(",").unwrap().chars().collect();
        let mut reader = LineReader::new("2,\"comma, value\",-3,-5.5,\"line1\nline2\",-1\n");
        let line = reader.read_line().unwrap();
        let values = split_line(&line, &delimiter, '"', &mut reader, 9).unwrap();
        assert_eq!(values.len(), 9);
        assert_eq!(
            texts(&values[..5]),
            vec![
                Some("2".into()),
                Some("comma, value".into()),
                Some("-3".into()),
                Some("-5.5".into()),
                Some(format!("line1{}line2", line_separator()))
            ]
        );
        // the remaining fields are padded with nulls, exactly like jackcess
        for value in &values[5..] {
            assert_eq!(*value, Value::Null);
        }
    }

    #[test]
    fn split_line_reports_unterminated_quote() {
        let delimiter: Vec<char> = delimiter_literal(",").unwrap().chars().collect();
        let mut reader = LineReader::new("\"a,b\n");
        let line = reader.read_line().unwrap();
        let err = split_line(&line, &delimiter, '"', &mut reader, 0).unwrap_err();
        assert!(format!("{err}").contains("Missing end of quoted value"));
    }

    #[test]
    fn line_reader_handles_all_terminators() {
        let mut reader = LineReader::new("a\r\nb\nc\rd");
        assert_eq!(reader.read_line().as_deref(), Some("a"));
        assert_eq!(reader.read_line().as_deref(), Some("b"));
        assert_eq!(reader.read_line().as_deref(), Some("c"));
        assert_eq!(reader.read_line().as_deref(), Some("d"));
        assert_eq!(reader.read_line(), None);

        let mut reader = LineReader::new("");
        assert_eq!(reader.read_line(), None);

        let mut reader = LineReader::new("\n");
        assert_eq!(reader.read_line().as_deref(), Some(""));
        assert_eq!(reader.read_line(), None);
    }

    #[test]
    fn big_decimal_parsing() {
        assert_eq!(parse_big_decimal("12.34").unwrap(), (1234, 2));
        assert_eq!(parse_big_decimal("-0.0001").unwrap(), (-1, 4));
        assert_eq!(parse_big_decimal("1e3").unwrap(), (1, -3));
        assert_eq!(parse_big_decimal("1.5E-2").unwrap(), (15, 3));
        assert_eq!(parse_big_decimal("+7").unwrap(), (7, 0));
        assert_eq!(rescale_decimal(12300, 4, 2).unwrap(), (123, 2));
        assert!(rescale_decimal(12345, 4, 2).is_err());
        assert_eq!(rescale_decimal(123, 2, 4).unwrap(), (12300, 4));
    }

    #[test]
    fn java_double_parsing() {
        assert_eq!(parse_java_double(" 1.5 ").unwrap(), 1.5);
        assert_eq!(parse_java_double("1e3").unwrap(), 1000.0);
        assert_eq!(parse_java_double("2.5d").unwrap(), 2.5);
        assert!(parse_java_double("Infinity").unwrap().is_infinite());
        assert!(parse_java_double("NaN").unwrap().is_nan());
        assert!(parse_java_double("x").is_err());

        // Java's grammar, not Rust's: `inf`/`nan` spellings are case sensitive
        // and cannot carry a type suffix; the hex float extension is accepted
        for text in ["inf", "INF", "nan", "NAN", "-NaNf", "1e", "1e+", ".", "1_0"] {
            let error = parse_java_double(text).expect_err(text);
            assert!(
                matches!(&error, Error::NumberFormat(message)
                    if message == &format!("For input string: \"{text}\"")),
                "{text}: unexpected error {error:?}"
            );
        }
        assert_eq!(
            parse_java_double("").unwrap_err().to_string(),
            "empty String"
        );
        assert_eq!(
            parse_java_double("   ").unwrap_err().to_string(),
            "empty String"
        );
        assert!(parse_java_double("+NaN").unwrap().is_nan());
        assert!(parse_java_double("-NaN").unwrap().is_nan());
        assert_eq!(parse_java_double("1.").unwrap(), 1.0);
        assert_eq!(parse_java_double("1.e3").unwrap(), 1000.0);
        assert_eq!(parse_java_double(".5").unwrap(), 0.5);
        assert_eq!(parse_java_double("-.5e-2").unwrap(), -0.005);
        assert_eq!(parse_java_double("\t2.5\n").unwrap(), 2.5);
        // hexadecimal floating point literals (`Double.valueOf` accepts them,
        // the binary exponent is mandatory)
        assert_eq!(parse_java_double("0x1p3").unwrap(), 8.0);
        assert_eq!(parse_java_double("0x1.8p1").unwrap(), 3.0);
        assert_eq!(parse_java_double("0X1.8P1").unwrap(), 3.0);
        assert_eq!(parse_java_double("0x.8p1").unwrap(), 1.0);
        assert_eq!(parse_java_double("0x1.p1").unwrap(), 2.0);
        assert_eq!(parse_java_double("0x1p-1").unwrap(), 0.5);
        assert_eq!(parse_java_double("0x1p3D").unwrap(), 8.0);
        for text in ["0x1.8", "0x1.8p", "0x1p", "0xp3"] {
            assert!(
                matches!(parse_java_double(text), Err(Error::NumberFormat(_))),
                "{text} must be rejected"
            );
        }
    }

    #[test]
    fn blank_and_whitespace_classification() {
        assert!(is_blank(""));
        assert!(is_blank(" \t\r\n"));
        assert!(!is_blank(" x "));
        assert!(!is_blank("\u{00A0}")); // NBSP is not Java whitespace
        assert!(is_java_whitespace('\u{3000}'));
    }

    #[test]
    fn reserved_words_are_escaped() {
        assert_eq!(escape_identifier("name"), "xname");
        assert_eq!(escape_identifier("Name"), "xName");
        assert_eq!(escape_identifier("Imported"), "Imported");
    }

    // ------------------------------------------------------------------
    // database round trips
    // ------------------------------------------------------------------

    fn sample_spec() -> TableSpec {
        TableSpec::new(
            "ExportData",
            vec![
                ColumnSpec::new("Num", DataType::Long),
                ColumnSpec::new("Label", DataType::Text).with_length(50),
                ColumnSpec::new("Qty", DataType::Int),
                ColumnSpec::new("Price", DataType::Money),
                ColumnSpec::new("Flag", DataType::Boolean),
                ColumnSpec::new("Stamp", DataType::ShortDateTime),
                ColumnSpec::new("Raw", DataType::Binary).with_length(30),
            ],
        )
    }

    fn sample_rows() -> Vec<Vec<Value>> {
        vec![
            vec![
                Value::Long(1),
                Value::Text("plain".into()),
                Value::Int(7),
                Value::Decimal {
                    unscaled: 123400,
                    scale: 4,
                },
                Value::Bool(true),
                Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0)),
                Value::Bytes(vec![1, 2, 3]),
            ],
            vec![
                Value::Long(2),
                Value::Text("comma,here".into()),
                Value::Int(-3),
                Value::Decimal {
                    unscaled: -55000,
                    scale: 4,
                },
                Value::Bool(false),
                Value::Null,
                Value::Bytes(vec![0, 255, 16]),
            ],
            vec![
                Value::Null,
                Value::Text("quote\"it's inside".into()),
                Value::Int(0),
                Value::Null,
                Value::Null,
                Value::DateTime(LocalDateTime::new(2021, 1, 2, 3, 4, 5, 0)),
                Value::Bytes(vec![]),
            ],
        ]
    }

    #[test]
    fn export_matches_expected_text() {
        let path = test_tmp_dir().join("export_basic.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&sample_spec()).unwrap();
        for row in sample_rows() {
            db.add_rows("ExportData", &[row]).unwrap();
        }

        let text = export_table(&mut db, "ExportData", &ExportOptions::default()).unwrap();
        let expected = [
            "1,plain,7,12.3400,true,2020-03-15T12:30:45,01 02 03",
            "2,\"comma,here\",-3,-5.5000,false,,00 FF 10",
            ",\"quote\"\"it's inside\",0,,false,2021-01-02T03:04:05,",
        ]
        .join(line_separator())
            + line_separator();
        assert_eq!(text, expected);
    }

    #[test]
    fn export_header_and_custom_delimiter() {
        let path = test_tmp_dir().join("export_header.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&sample_spec()).unwrap();
        for row in sample_rows() {
            db.add_rows("ExportData", &[row]).unwrap();
        }

        let opts = ExportOptions::new()
            .with_header(true)
            .with_delimiter(";")
            .with_quote('\'');
        let text = export_table(&mut db, "ExportData", &opts).unwrap();
        let first_line = format!("Num;Label;Qty;Price;Flag;Stamp;Raw{}", line_separator());
        assert!(text.starts_with(&first_line));
        assert!(text.contains("2;comma,here;-3"));
        assert!(text.contains("'quote\"it''s inside'"));
    }

    struct DropAndSkipFilter;

    impl ExportFilter for DropAndSkipFilter {
        fn filter_columns(&self, columns: &[ExportColumn]) -> Vec<ExportColumn> {
            columns
                .iter()
                .filter(|c| c.name != "Label")
                .cloned()
                .collect()
        }

        fn filter_row(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
            // row is (Num, Qty, Price, Flag, Stamp, Raw)
            if matches!(row.first(), Some(Value::Long(2))) {
                return Ok(None);
            }
            let mut row = row.to_vec();
            if let Some(Value::Long(num)) = row.first_mut() {
                *num += 100;
            }
            Ok(Some(row))
        }
    }

    #[test]
    fn export_column_and_row_filters() {
        let path = test_tmp_dir().join("export_filter.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&sample_spec()).unwrap();
        for row in sample_rows() {
            db.add_rows("ExportData", &[row]).unwrap();
        }

        let opts = ExportOptions::new()
            .with_header(true)
            .with_filter(Box::new(DropAndSkipFilter));
        let text = export_table(&mut db, "ExportData", &opts).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "Num,Qty,Price,Flag,Stamp,Raw");
        assert_eq!(lines.len(), 3); // header + rows 1 and 3 (row 2 skipped)
        assert_eq!(lines[1], "101,7,12.3400,true,2020-03-15T12:30:45,01 02 03");
        assert_eq!(lines[2], ",0,,false,2021-01-02T03:04:05,");
    }

    #[test]
    fn import_round_trip_creates_rows() {
        let export_path = test_tmp_dir().join("import_source.mdb");
        let import_path = test_tmp_dir().join("import_target.mdb");
        let _ = std::fs::remove_file(&export_path);
        let _ = std::fs::remove_file(&import_path);
        let mut db = Database::create(FileFormat::V2003, &export_path).unwrap();
        db.create_table(&sample_spec()).unwrap();
        for row in sample_rows() {
            db.add_rows("ExportData", &[row]).unwrap();
        }
        let text = export_table(
            &mut db,
            "ExportData",
            &ExportOptions::new().with_header(true),
        )
        .unwrap();

        let mut target = Database::create(FileFormat::V2003, &import_path).unwrap();
        let opts = ImportOptions::new("Imported")
            .with_column_types(vec![
                ImportColumn::new(DataType::Long),
                ImportColumn::new(DataType::Text).with_length(50),
                ImportColumn::new(DataType::Int),
                ImportColumn::new(DataType::Money),
                ImportColumn::new(DataType::Boolean),
                ImportColumn::new(DataType::ShortDateTime),
                ImportColumn::new(DataType::Binary).with_length(30),
            ])
            .with_filter(Box::new(DateParsingFilter));
        let name = import_text(&text, &mut target, &opts).unwrap().unwrap();
        assert_eq!(name, "Imported");

        let page = target.get_table("Imported").unwrap().table_def_page_number;
        let rows = target.read_table_rows(page).unwrap();
        let rows: Vec<Vec<Value>> = rows.into_iter().flatten().collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Long(1));
        assert_eq!(rows[0][1], Value::Text("plain".into()));
        assert_eq!(rows[1][1], Value::Text("comma,here".into()));
        assert_eq!(rows[2][1], Value::Text("quote\"it's inside".into()));
        assert_eq!(rows[1][5], Value::Null);
        assert_eq!(
            rows[2][5],
            Value::DateTime(LocalDateTime::new(2021, 1, 2, 3, 4, 5, 0))
        );
        assert_eq!(rows[0][6], Value::Bytes(vec![1, 2, 3]));
        assert_eq!(rows[1][6], Value::Bytes(vec![0, 255, 16]));
    }

    /// Converts the export representation back to stored values: an ISO
    /// date/time in column 5, a hex dump in column 6, and empty fields in the
    /// non textual columns to nulls (the terser equivalent of the Java import
    /// filter used by the verification harness).
    struct DateParsingFilter;

    impl ImportFilter for DateParsingFilter {
        fn filter_row(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
            let mut row = row.to_vec();
            for (i, value) in row.iter_mut().enumerate() {
                match i {
                    // Label (1) keeps empty strings as zero length text.
                    5 => {
                        if let Value::Text(text) = value {
                            if !text.is_empty() {
                                *value = Value::DateTime(parse_iso_local_date_time(text)?);
                            } else {
                                *value = Value::Null;
                            }
                        }
                    }
                    6 => {
                        if let Value::Text(text) = value {
                            *value = if text.is_empty() {
                                Value::Null
                            } else {
                                Value::Bytes(parse_hex_dump(text)?)
                            };
                        }
                    }
                    0 | 2 | 3 | 4 => {
                        if matches!(value, Value::Text(text) if text.is_empty()) {
                            *value = Value::Null;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Some(row))
        }
    }

    fn parse_hex_dump(text: &str) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        for token in text.split_whitespace() {
            if token.len() != 2 {
                return Err(Error::InvalidValue(format!("invalid hex byte '{token}'")));
            }
            let byte = u8::from_str_radix(token, 16)
                .map_err(|_| Error::InvalidValue(format!("invalid hex byte '{token}'")))?;
            bytes.push(byte);
        }
        Ok(bytes)
    }

    #[test]
    fn import_batches_and_null_handling() {
        let path = test_tmp_dir().join("import_batch.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("A", DataType::Long),
                ColumnSpec::new("B", DataType::Text).with_length(20),
            ],
        ))
        .unwrap();

        // 205 rows -> batches of 200 then 5
        let mut text = String::new();
        for i in 0..205 {
            text.push_str(&format!("{i},row{i}{}", line_separator()));
        }
        let opts = ImportOptions::new("T")
            .with_use_existing_table(true)
            .with_header(false);
        import_text(&text, &mut db, &opts).unwrap().unwrap();

        let page = db.get_table("T").unwrap().table_def_page_number;
        let rows = db.read_table_rows(page).unwrap();
        let rows: Vec<Vec<Value>> = rows.into_iter().flatten().collect();
        assert_eq!(rows.len(), 205);
        assert_eq!(rows[0], vec![Value::Long(0), Value::Text("row0".into())]);
        // the empty text column stays empty; a missing trailing field is null
        assert_eq!(
            rows[204],
            vec![Value::Long(204), Value::Text("row204".into())]
        );
    }

    #[test]
    fn import_missing_trailing_field_is_null() {
        let path = test_tmp_dir().join("import_nulls.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("A", DataType::Text).with_length(20),
                ColumnSpec::new("B", DataType::Text).with_length(20),
            ],
        ))
        .unwrap();
        let opts = ImportOptions::new("T")
            .with_use_existing_table(true)
            .with_header(false);
        import_text("1,\r\n2,x\r\n", &mut db, &opts).unwrap();
        let page = db.get_table("T").unwrap().table_def_page_number;
        let rows: Vec<Vec<Value>> = db
            .read_table_rows(page)
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(rows[0], vec![Value::Text("1".into()), Value::Null]);
        assert_eq!(
            rows[1],
            vec![Value::Text("2".into()), Value::Text("x".into())]
        );
    }

    #[test]
    fn import_text_into_binary_and_ole_columns_writes_the_java_serialized_string() {
        let path = test_tmp_dir().join("import_binary.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&TableSpec::new(
            "B",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Raw", DataType::Binary).with_length(64),
                ColumnSpec::new("Ole", DataType::Ole),
            ],
        ))
        .unwrap();
        let opts = ImportOptions::new("B")
            .with_use_existing_table(true)
            .with_header(false);
        import_text("1,hello,a\u{0}b\n", &mut db, &opts).unwrap();
        let page = db.get_table("B").unwrap().table_def_page_number;
        let rows: Vec<Vec<Value>> = db
            .read_table_rows(page)
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(rows.len(), 1);
        // `ObjectOutputStream.writeObject(String)`: header, TC_STRING, length
        // and the modified UTF-8 bytes
        assert_eq!(
            rows[0][1],
            Value::Bytes(vec![
                0xAC, 0xED, 0x00, 0x05, 0x74, 0x00, 0x05, b'h', b'e', b'l', b'l', b'o'
            ])
        );
        assert_eq!(rows[0][1], Value::Bytes(java_serialize_string("hello")));
        // U+0000 is C0 80 in the modified UTF-8 form
        assert_eq!(rows[0][2], Value::Bytes(java_serialize_string("a\u{0}b")));
    }

    #[test]
    fn import_text_into_big_binary_parses_the_number_like_java() {
        let path = test_tmp_dir().join("import_bigbin.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        db.create_table(&TableSpec::new(
            "B",
            vec![
                ColumnSpec::new("ID", DataType::Long),
                ColumnSpec::new("Ref", DataType::BigBinary),
            ],
        ))
        .unwrap();
        let opts = ImportOptions::new("B")
            .with_use_existing_table(true)
            .with_header(false);
        // `writeFixedLengthField` -> `toNumber(text).intValue()`: the stored
        // bytes are Java's 4 byte int of `(int)Double.valueOf(text)`
        let text = "1,12\n2,-7\n3,3.9\n4,1e3\n5, 42 \n6,NaN\n7,Infinity\n\
                    8,2147483648\n9,-2147483649\n10,0x1p3\n11,0x1.8p1\n";
        import_text(text, &mut db, &opts).unwrap();
        let page = db.get_table("B").unwrap().table_def_page_number;
        let rows: Vec<Vec<Value>> = db
            .read_table_rows(page)
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        let expected: [i32; 11] = [12, -7, 3, 1000, 42, 0, i32::MAX, i32::MAX, i32::MIN, 8, 3];
        assert_eq!(rows.len(), expected.len());
        // the value is the column's whole 3992 byte fixed slot: Java's
        // `DataType.BIG_BINARY.getFixedSize(...)` is 3992, `ColumnImpl.read`
        // returns all of it and `writeFixedLengthField` writes the 4 byte int
        // at the front (the rest of a new row stays zero)
        for (row, value) in rows.iter().zip(expected) {
            let mut bytes = value.to_le_bytes().to_vec();
            bytes.resize(3992, 0);
            assert_eq!(row[1], Value::Bytes(bytes));
        }

        // a text which is not a number fails with Java's exact exception and
        // writes none of the pending rows.  `ImportUtil` imports in batches, so
        // the failure surfaces as `TableImpl.addRows`' `BatchUpdateException`
        // (updateCount 0: the failing row is the first of its batch)
        let error = import_text("1,abc\n", &mut db, &opts).unwrap_err();
        assert!(error.is_batch_update());
        assert_eq!(
            error.to_string(),
            "Failed adding rows (Db=import_bigbin.mdb;Table=B): \
             java.lang.NumberFormatException: For input string: \"abc\""
        );
        assert_eq!(error.update_count(), Some(0));
        let cause = error.batch_cause().expect("the batch cause");
        assert!(matches!(cause, Error::NumberFormat(_)));
        assert_eq!(
            cause.java_to_string(),
            "java.lang.NumberFormatException: For input string: \"abc\""
        );
        // a quoted empty value is the empty string (not a missing field, which
        // the tokenizer pads with null)
        let error = import_text("1,\"\"\n", &mut db, &opts).unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("java.lang.NumberFormatException: empty String"),
            "unexpected error: {error}"
        );
        assert_eq!(
            db.read_table_rows(page)
                .unwrap()
                .into_iter()
                .flatten()
                .count(),
            expected.len()
        );
    }

    #[test]
    fn import_blank_first_line_is_a_no_op() {
        let path = test_tmp_dir().join("import_blank.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let opts = ImportOptions::new("Nothing");
        assert_eq!(import_text("   \r\n", &mut db, &opts).unwrap(), None);
        assert!(db.find_table("Nothing").unwrap().is_none());
    }

    #[test]
    fn import_unique_table_name() {
        let path = test_tmp_dir().join("import_unique.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let opts = ImportOptions::new("Same");
        assert_eq!(
            import_text("A,B\r\n1,2\r\n", &mut db, &opts).unwrap(),
            Some("Same".to_string())
        );
        assert_eq!(
            import_text("A,B\r\n1,2\r\n", &mut db, &opts).unwrap(),
            Some("Same2".to_string())
        );
    }

    #[test]
    fn import_default_columns_are_text_255() {
        let path = test_tmp_dir().join("import_default_types.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let name = import_text("Alpha,Beta\r\nx,y\r\n", &mut db, &ImportOptions::new("T"))
            .unwrap()
            .unwrap();
        assert_eq!(name, "T");
        let table = db.get_table("T").unwrap();
        assert_eq!(table.columns.len(), 2);
        for (column, expected) in table.columns.iter().zip(["Alpha", "Beta"]) {
            assert_eq!(column.name, expected);
            assert_eq!(column.data_type, DataType::Text);
            assert_eq!(column.get_length(), 255);
        }
    }

    struct LongFirstColumn;

    impl ImportFilter for LongFirstColumn {
        fn filter_columns(&self, columns: &mut Vec<ImportColumn>) -> Result<()> {
            if let Some(first) = columns.first_mut() {
                *first = ImportColumn::new(DataType::Long);
            }
            Ok(())
        }
    }

    #[test]
    fn import_filter_columns_and_rows() {
        let path = test_tmp_dir().join("import_filter.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let opts = ImportOptions::new("T")
            .with_filter(Box::new(LongFirstColumn))
            .with_header(true);
        // a header, then a row with a trailing (missing) field
        import_text("A,B\r\n10,kept\r\n", &mut db, &opts).unwrap();
        let page = db.get_table("T").unwrap().table_def_page_number;
        assert_eq!(
            db.get_table("T").unwrap().columns[0].data_type,
            DataType::Long
        );
        let rows: Vec<Vec<Value>> = db
            .read_table_rows(page)
            .unwrap()
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(rows[0], vec![Value::Long(10), Value::Text("kept".into())]);
    }

    #[test]
    fn import_text_into_date_column_needs_a_filter() {
        let path = test_tmp_dir().join("import_date_error.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let opts = ImportOptions::new("T")
            .with_column_types(vec![ImportColumn::new(DataType::ShortDateTime)]);
        let err = import_text("Stamp\r\n2020-03-15T12:30:45\r\n", &mut db, &opts).unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("cannot be imported from text"),
            "{message}"
        );
    }

    /// Minimal `LocalDateTime.parse` for the ISO representation used by the
    /// export (`yyyy-MM-ddTHH:mm[:ss[.fff...]]`).
    fn parse_iso_local_date_time(text: &str) -> Result<LocalDateTime> {
        let bytes = text.as_bytes();
        let parse = |range: std::ops::Range<usize>| -> Result<i64> {
            text.get(range)
                .and_then(|s| s.parse::<i64>().ok())
                .ok_or_else(|| Error::InvalidValue(format!("invalid date '{text}'")))
        };
        if bytes.len() < 16 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
            return Err(Error::InvalidValue(format!("invalid date '{text}'")));
        }
        let year = parse(0..4)? as i32;
        let month = parse(5..7)? as u32;
        let day = parse(8..10)? as u32;
        let hour = parse(11..13)? as u32;
        let minute = parse(14..16)? as u32;
        let mut second = 0u32;
        let mut nano = 0u32;
        if bytes.len() >= 19 && bytes[16] == b':' {
            second = parse(17..19)? as u32;
            if bytes.len() > 19 && bytes[19] == b'.' {
                let fraction = &text[20..];
                let digits = fraction.len();
                let value: u32 = fraction
                    .parse()
                    .map_err(|_| Error::InvalidValue(format!("invalid date '{text}'")))?;
                nano = match digits {
                    3 => value * 1_000_000,
                    6 => value * 1_000,
                    9 => value,
                    _ => return Err(Error::InvalidValue(format!("invalid date '{text}'"))),
                };
            }
        }
        Ok(LocalDateTime::new(
            year, month, day, hour, minute, second, nano,
        ))
    }

    // ------------------------------------------------------------------
    // exportAll
    // ------------------------------------------------------------------

    /// `ExportUtil.exportAll` maps a table name to `tableName + "." + ext`
    /// with no escaping or replacement at all (Java's `new File(dir, ...)`).
    #[test]
    fn export_all_file_names_match_java() {
        assert_eq!(export_file_name("Customers", "csv"), "Customers.csv");
        assert_eq!(
            export_file_name("R&D #2 (100%)", "txt"),
            "R&D #2 (100%).txt"
        );
        assert_eq!(export_file_name("Ünïcode 表", "csv"), "Ünïcode 表.csv");
        // a dot in the table name simply produces two dots (Access rejects
        // dots in identifiers, but the mapping itself is literal)
        assert_eq!(export_file_name("dots.name", "csv"), "dots.name.csv");
        // an empty extension leaves the trailing dot
        assert_eq!(export_file_name("Table", ""), "Table.");
        // path separators are *not* replaced: they become path components
        assert_eq!(export_file_name("a/b", "csv"), "a/b.csv");
        assert_eq!(export_file_name("a\\b", "csv"), "a\\b.csv");
        if cfg!(windows) {
            // ... and therefore both names resolve to the same file on Windows
            assert_eq!(
                Path::new("a/b.csv"),
                Path::new("a\\b.csv"),
                "Windows treats both separators alike"
            );
        } else {
            // ... while a backslash stays a literal character on Unix
            assert_ne!(Path::new("a/b.csv"), Path::new("a\\b.csv"));
        }
    }

    fn export_all_tmp_dir(name: &str) -> std::path::PathBuf {
        let dir = test_tmp_dir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn make_all_db(name: &str) -> (Database, std::path::PathBuf) {
        let path = test_tmp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        let first = TableSpec::new(
            "Customers",
            vec![
                ColumnSpec::new("Id", DataType::Long),
                ColumnSpec::new("Name", DataType::Text).with_length(50),
                ColumnSpec::new("Note", DataType::Memo),
                ColumnSpec::new("Stamp", DataType::ShortDateTime),
                ColumnSpec::new("Raw", DataType::Binary).with_length(10),
            ],
        );
        db.create_table(&first).unwrap();
        db.create_table(&TableSpec::new(
            "Empty",
            vec![
                ColumnSpec::new("A", DataType::Long),
                ColumnSpec::new("B", DataType::Text).with_length(20),
            ],
        ))
        .unwrap();
        db.create_table(&TableSpec::new(
            "R&D #2 (100%)",
            vec![
                ColumnSpec::new("A", DataType::Long),
                ColumnSpec::new("Label", DataType::Text).with_length(50),
            ],
        ))
        .unwrap();
        let rows = vec![
            vec![
                Value::Long(1),
                Value::Text("plain".into()),
                Value::Text("memo one".into()),
                Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0)),
                Value::Bytes(vec![1, 2, 3]),
            ],
            vec![
                Value::Long(2),
                Value::Text("comma,here".into()),
                Value::Text("line1\nline2".into()),
                Value::Null,
                Value::Bytes(vec![0, 255]),
            ],
            vec![
                Value::Null,
                Value::Text("quote\"it's".into()),
                Value::Null,
                Value::DateTime(LocalDateTime::new(2021, 1, 2, 3, 4, 5, 0)),
                Value::Null,
            ],
        ];
        for row in rows {
            db.add_rows("Customers", &[row]).unwrap();
        }
        db.add_rows(
            "R&D #2 (100%)",
            &[vec![Value::Long(1), Value::Text("weird".into())]],
        )
        .unwrap();
        (db, path)
    }

    #[test]
    fn export_all_exports_every_normal_table_and_skips_system_tables() {
        let dir = export_all_tmp_dir("exportall_basic");
        let (mut db, _path) = make_all_db("exportall_basic.mdb");

        export_all(&mut db, &dir).unwrap();

        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                "Customers.csv".to_string(),
                "Empty.csv".to_string(),
                "R&D #2 (100%).csv".to_string(),
            ],
            "only the three user tables are exported"
        );
        // the system tables of the database are present but not exported
        assert!(!dir.join("MSysObjects.csv").exists());
        assert!(!dir.join("MSysACEs.csv").exists());

        // the Customers file is exactly the single table export
        let expected = export_table(&mut db, "Customers", &ExportOptions::default()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("Customers.csv")).unwrap(),
            expected
        );
        // an empty table still produces an empty file (and a header when set)
        assert_eq!(
            std::fs::read(dir.join("Empty.csv")).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn export_all_honours_extension_header_delimiter_quote_and_filter() {
        let dir = export_all_tmp_dir("exportall_options");
        let (mut db, _path) = make_all_db("exportall_options.mdb");

        struct DropNote;
        impl ExportFilter for DropNote {
            fn filter_columns(&self, columns: &[ExportColumn]) -> Vec<ExportColumn> {
                columns
                    .iter()
                    .filter(|c| c.name != "Note")
                    .cloned()
                    .collect()
            }

            fn filter_row(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
                if row.first().map(Value::is_null).unwrap_or(false) {
                    return Ok(None);
                }
                Ok(Some(row.to_vec()))
            }
        }

        let opts = ExportOptions::new()
            .with_header(true)
            .with_delimiter(";")
            .with_quote('\'')
            .with_filter(Box::new(DropNote));
        export_all_with(&mut db, &dir, "txt", &opts).unwrap();

        let customers = std::fs::read_to_string(dir.join("Customers.txt")).unwrap();
        let first_line = format!("Id;Name;Stamp;Raw{}", line_separator());
        assert!(customers.starts_with(&first_line), "{customers:?}");
        // the Note column is gone, the null Id row is skipped, and the values
        // follow the quote/delimiter options
        assert!(customers.contains("1;plain;2020-03-15T12:30:45;01 02 03"));
        assert!(customers.contains("2;comma,here;;00 FF"));
        assert_eq!(
            customers.lines().count(),
            3,
            "header plus the two rows with a non null Id"
        );
        // the filter drops a column which only some tables have
        assert_eq!(
            std::fs::read_to_string(dir.join("R&D #2 (100%).txt")).unwrap(),
            format!("A;Label{}1;weird{}", line_separator(), line_separator())
        );
        assert!(!dir.join("Customers.csv").exists());
    }

    #[test]
    fn export_all_and_export_file_always_overwrite_existing_files() {
        let dir = export_all_tmp_dir("exportall_overwrite");
        let (mut db, _path) = make_all_db("exportall_overwrite.mdb");

        // a long stale file must be truncated, not appended to and not refused
        let stale = vec![b'X'; 100_000];
        std::fs::write(dir.join("Customers.csv"), &stale).unwrap();
        std::fs::write(dir.join("Empty.csv"), &stale).unwrap();
        export_all(&mut db, &dir).unwrap();
        let expected = export_table(&mut db, "Customers", &ExportOptions::default()).unwrap();
        assert_eq!(
            std::fs::read(dir.join("Customers.csv")).unwrap(),
            expected.as_bytes()
        );
        assert_eq!(
            std::fs::read(dir.join("Empty.csv")).unwrap(),
            Vec::<u8>::new()
        );

        // the single table export has the same truncating semantics
        std::fs::write(dir.join("single.csv"), &stale).unwrap();
        export_table_to_file(
            &mut db,
            "Customers",
            &dir.join("single.csv"),
            &ExportOptions::default(),
        )
        .unwrap();
        assert_eq!(
            std::fs::read(dir.join("single.csv")).unwrap(),
            expected.as_bytes()
        );
    }

    /// `exportWriter` writes to the writer it is given; a writer opened in
    /// append mode therefore appends the export to the existing bytes (this is
    /// the only append behaviour 5.0.4 offers: `exportAll` has no append flag
    /// and always truncates).
    #[test]
    fn export_table_to_writer_appends_to_an_append_writer() {
        let dir = export_all_tmp_dir("exportall_append");
        let (mut db, _path) = make_all_db("exportall_append.mdb");
        let path = dir.join("append.csv");
        std::fs::write(&path, b"PREFIX\n").unwrap();

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        let mut out = std::io::BufWriter::new(file);
        export_table_to_writer(
            &mut db,
            "Customers",
            &mut out,
            &ExportOptions::new().with_header(true),
        )
        .unwrap();
        drop(out);

        let expected_export = export_table(
            &mut db,
            "Customers",
            &ExportOptions::new().with_header(true),
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("PREFIX\n{expected_export}")
        );

        // a second exportAll run replaces the file (it never appends)
        export_all(&mut db, &dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("Customers.csv")).unwrap(),
            export_table(&mut db, "Customers", &ExportOptions::default()).unwrap()
        );
    }

    /// `exportAll` does not escape table names, so two names which map to the
    /// same platform path write the same file; the later table wins, exactly
    /// like the Java loop.  On Windows `a/b` and `a\b` both name `a\b`.
    #[test]
    #[cfg(windows)]
    fn export_all_file_name_collision_overwrites_like_java() {
        let dir = export_all_tmp_dir("exportall_collision");
        std::fs::create_dir_all(dir.join("a")).unwrap();
        let path = test_tmp_dir().join("exportall_collision.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        for name in ["a/b", "a\\b"] {
            db.create_table(&TableSpec::new(
                name,
                vec![
                    ColumnSpec::new("Id", DataType::Long),
                    ColumnSpec::new("Name", DataType::Text).with_length(20),
                ],
            ))
            .unwrap();
        }
        db.add_rows("a/b", &[vec![Value::Long(1), Value::Text("first".into())]])
            .unwrap();
        db.add_rows(
            "a\\b",
            &[vec![Value::Long(2), Value::Text("second".into())]],
        )
        .unwrap();

        export_all(&mut db, &dir).unwrap();

        // case insensitive order: "a/b" before "a\b" ('/' < '\'), so the
        // second table's export is what remains in the shared file
        assert_eq!(
            std::fs::read_to_string(dir.join("a").join("b.csv")).unwrap(),
            format!("2,second{}", line_separator())
        );
    }

    #[test]
    fn export_all_to_map_matches_the_directory_output() {
        let dir = export_all_tmp_dir("exportall_map");
        let (mut db, _path) = make_all_db("exportall_map.mdb");
        let opts = ExportOptions::new().with_header(true).with_delimiter("|");

        let map = export_all_to_map(&mut db, &opts).unwrap();
        let order = db.get_table_names().unwrap();
        assert_eq!(
            order,
            vec![
                "Customers".to_string(),
                "Empty".to_string(),
                "R&D #2 (100%)".to_string(),
            ]
        );
        assert_eq!(map.len(), order.len());
        assert!(!map.contains_key("MSysObjects"));
        for table in &order {
            let expected = export_table(&mut db, table, &opts).unwrap();
            assert_eq!(map.get(table), Some(&expected));
        }

        export_all_with(&mut db, &dir, "csv", &opts).unwrap();
        for table in &order {
            assert_eq!(
                std::fs::read_to_string(dir.join(export_file_name(table, "csv"))).unwrap(),
                map[table]
            );
        }
    }

    #[test]
    fn export_all_on_an_empty_database_writes_nothing() {
        let dir = export_all_tmp_dir("exportall_empty");
        let path = test_tmp_dir().join("exportall_empty.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        assert!(db.get_table_names().unwrap().is_empty());
        export_all(&mut db, &dir).unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert!(export_all_to_map(&mut db, &ExportOptions::default())
            .unwrap()
            .is_empty());
    }

    /// A table name with a character Windows forbids in a file name is not
    /// escaped or replaced; the create fails like Java's `InvalidPathException`
    /// (the earlier files of the same `exportAll` call stay in place).
    #[test]
    #[cfg(windows)]
    fn export_all_does_not_escape_illegal_path_characters() {
        let dir = export_all_tmp_dir("exportall_illegal");
        let path = test_tmp_dir().join("exportall_illegal.mdb");
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(FileFormat::V2003, &path).unwrap();
        for name in ["Alpha", "Questions?Mark", "Zulu"] {
            db.create_table(&TableSpec::new(
                name,
                vec![ColumnSpec::new("A", DataType::Long)],
            ))
            .unwrap();
            db.add_rows(name, &[vec![Value::Long(1)]]).unwrap();
        }
        // case insensitive order: Alpha, Questions?Mark, Zulu.  The OS error
        // text is localized, so only the failure itself is asserted (Java
        // throws an unchecked InvalidPathException here).
        let err = export_all(&mut db, &dir).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "unexpected error: {err}");
        assert!(dir.join("Alpha.csv").exists(), "earlier files remain");
        assert!(
            !dir.join("Zulu.csv").exists(),
            "the loop stops at the failure"
        );
    }
}
