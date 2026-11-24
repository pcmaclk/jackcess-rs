//! `realquery` - dumps the query model and table metadata of an
//! Access-created database in the canonical text form
//! `java-src/RealQueryDump.java` also writes.
//!
//! Usage:  realquery <db> <out> [--rows] [--tables]
//!
//! The dump is the verification artifact of `tools/verify-realquery.ps1`: the
//! Java tool and this binary read the same real (Access-created) database and
//! must produce byte-identical text.  Every record is one line; string values
//! are escaped (`\\`, `\t`, `\r`, `\n`, `\0`) and `<null>` marks SQL `NULL`.
//!
//! Records:
//!
//! * `QUERY|...` and `SQL|...`: what `Database::get_queries()` exposes for
//!   every query (name, type, flags, owner access, parameters and
//!   `to_sql_string()`), with `SQLERROR|...` when rendering fails.
//! * `ROW|...` (`--rows`): the raw `MSysQueries` rows, in cursor order.
//! * `TABLE|...`, `COLUMN|...`, `INDEX|...`, `ROWS|...` and `OPEN|...`
//!   (`--tables`): the metadata of every non-system table, its definition
//!   columns/indexes and the "no data access" error of a linked ODBC table.

use std::fmt::Write as _;

use jackcess::database::{Database, TableMetaData};
use jackcess::error::Error;
use jackcess::query::Query;
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: realquery <db> <out> [--rows] [--tables]");
        std::process::exit(2);
    }
    let mut dump_rows = false;
    let mut dump_tables = false;
    for arg in &args[3..] {
        match arg.as_str() {
            "--rows" => dump_rows = true,
            "--tables" => dump_tables = true,
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }

    let mut out = String::from("JACKCESS-REALQUERY 1\n");
    match Database::open_path(std::path::Path::new(&args[1])) {
        Err(error) => {
            let _ = writeln!(
                out,
                "FATAL|{}|{}",
                error_tag(&error),
                esc(&error_message(&error))
            );
        }
        Ok(mut db) => {
            // a failure after some records keeps the records written so far,
            // like the Java tool's catch around the whole dump
            if let Err(error) = dump(&mut db, &mut out, dump_rows, dump_tables) {
                let _ = writeln!(
                    out,
                    "FATAL|{}|{}",
                    error_tag(&error),
                    esc(&error_message(&error))
                );
            }
        }
    }
    if let Err(error) = std::fs::write(&args[2], out) {
        eprintln!("error writing {}: {}", args[2], error);
        std::process::exit(1);
    }
}

fn dump(
    db: &mut Database,
    out: &mut String,
    dump_rows: bool,
    dump_tables: bool,
) -> jackcess::Result<()> {
    let queries = db.get_queries()?;
    for query in &queries {
        append_query(out, query)?;
    }
    if dump_rows {
        append_query_rows(db, out)?;
    }
    if dump_tables {
        append_tables(db, out)?;
    }
    Ok(())
}

fn append_query(out: &mut String, query: &Query) -> jackcess::Result<()> {
    let owner_access = match query.get_owner_access_type() {
        Ok(access) => access.to_string(),
        Err(error) => format!("ERROR:{}", error_tag(&error)),
    };
    let parameters = match query.get_parameters() {
        Ok(parameters) => parameters.join("|"),
        Err(error) => format!("ERROR:{}", error_tag(&error)),
    };
    let _ = writeln!(
        out,
        "QUERY|{}|type={}|objectFlag={}|objectId={}|hidden={}|ownerAccess={}|parameters={}",
        esc(query.name()),
        query.query_type().name(),
        query.object_flag(),
        query.object_id(),
        query.is_hidden(),
        esc(&owner_access),
        esc(&parameters)
    );
    match query.to_sql_string() {
        Ok(sql) => {
            let _ = writeln!(out, "SQL|{}", esc(&sql));
        }
        Err(error) => {
            let _ = writeln!(
                out,
                "SQLERROR|{}|{}",
                error_tag(&error),
                esc(&error_message(&error))
            );
        }
    }
    Ok(())
}

fn append_query_rows(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    let Some(meta) = db.get_table_meta_data("MSysQueries")? else {
        return Ok(());
    };
    let Some(page) = meta.page_number else {
        return Ok(());
    };
    let rows = db.read_table_rows(page)?;
    let table = db.tables.get(&page).expect("MSysQueries table");
    let idx_object_id = table.column_index("ObjectId");
    let idx_attribute = table.column_index("Attribute");
    let idx_expression = table.column_index("Expression");
    let idx_flag = table.column_index("Flag");
    let idx_extra = table.column_index("LvExtra");
    let idx_name1 = table.column_index("Name1");
    let idx_name2 = table.column_index("Name2");
    let idx_order = table.column_index("Order");

    for row in rows.into_iter().flatten() {
        let field = |index: Option<usize>| value_field(row.get(index.unwrap_or(usize::MAX)));
        let _ = writeln!(
            out,
            "ROW|objectId={}|attribute={}|expression={}|flag={}|extra={}|name1={}|name2={}|order={}",
            field(idx_object_id),
            field(idx_attribute),
            field(idx_expression),
            field(idx_flag),
            field(idx_extra),
            field(idx_name1),
            field(idx_name2),
            field(idx_order)
        );
    }
    Ok(())
}

fn append_tables(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    let mut names = db.get_table_names()?;
    names.sort();

    for name in names {
        let meta = match db.get_table_meta_data(&name) {
            Ok(Some(meta)) => meta,
            Ok(None) => continue,
            Err(error) => {
                let _ = writeln!(
                    out,
                    "TABLEERROR|{}|{}|{}",
                    esc(&name),
                    error_tag(&error),
                    esc(&error_message(&error))
                );
                continue;
            }
        };
        append_table_metadata(out, &meta);
        append_table_details(db, out, &meta)?;
        append_table_open(db, out, &meta)?;
    }
    Ok(())
}

fn append_table_metadata(out: &mut String, meta: &TableMetaData) {
    let _ = writeln!(
        out,
        "TABLE|{}|type={}|linked={}|system={}|linkedTable={}|linkedDb={}|connection={}|created={}|updated={}|page={}",
        esc(&meta.name),
        meta.table_type.name(),
        meta.is_linked(),
        meta.is_system(),
        esc_option(meta.linked_table_name.as_deref()),
        esc_option(meta.linked_db_name.as_deref()),
        esc_option(meta.connection_name.as_deref()),
        format_date(meta.created_date),
        format_date(meta.updated_date),
        meta.page_number
            .map(|page| page.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
}

fn append_table_details(
    db: &mut Database,
    out: &mut String,
    meta: &TableMetaData,
) -> jackcess::Result<()> {
    let page = match db.find_table(&meta.name) {
        Ok(Some(table)) => table.table_def_page_number,
        Ok(None) => return Ok(()),
        Err(error) => {
            let _ = writeln!(
                out,
                "TABLEOPEN|{}|{}|{}",
                esc(&meta.name),
                error_tag(&error),
                esc(&error_message(&error))
            );
            return Ok(());
        }
    };
    let columns = match db.get_column_meta_data_all(&meta.name) {
        Ok(columns) => columns,
        Err(error) => {
            let _ = writeln!(
                out,
                "TABLEMETA|{}|{}|{}",
                esc(&meta.name),
                error_tag(&error),
                esc(&error_message(&error))
            );
            return Ok(());
        }
    };
    // a linked (type 6) table resolves into the linkee database, whose table is
    // not in this database's table map; the linked ODBC case is local
    let Some(table) = db.tables.get(&page) else {
        return Ok(());
    };
    let _ = writeln!(
        out,
        "DATE|{}|created={}|updated={}",
        esc(&table.name),
        format_date(table.get_created_date()),
        format_date(table.get_updated_date())
    );
    for column in &columns {
        let _ = writeln!(
            out,
            "COLUMN|{}|{}|type={}|sqlType={}|length={}|units={}|precision={}|scale={}|autoNumber={}|hidden={}|calculated={}|appendOnly={}|variable={}",
            esc(&table.name),
            esc(&column.name),
            column.data_type.get_name(),
            column
                .sql_type
                .map(|sql_type| sql_type.to_string())
                .unwrap_or_else(|| "unsupported".to_string()),
            column.length,
            column.length_in_units,
            column.precision,
            column.scale,
            column.auto_number,
            column.hidden,
            column.calculated,
            column.append_only,
            column.variable_length
        );
    }
    for index in &table.indexes {
        let index_meta = table.index_meta_data(index);
        let columns = index_meta
            .columns
            .iter()
            .zip(index_meta.ascending.iter())
            .map(|(name, ascending)| format!("{}:{}", esc(name), ascending))
            .collect::<Vec<_>>()
            .join(",");
        let _ = writeln!(
            out,
            "INDEX|{}|{}|primary={}|foreign={}|unique={}|required={}|ignoreNulls={}|columns={}",
            esc(&table.name),
            esc(&index_meta.name),
            index_meta.primary_key,
            index_meta.foreign_key,
            index_meta.unique,
            index_meta.required,
            index_meta.ignore_nulls,
            columns
        );
    }
    let _ = writeln!(out, "ROWS|{}|{}", esc(&table.name), table.get_row_count());
    Ok(())
}

fn append_table_open(
    db: &mut Database,
    out: &mut String,
    meta: &TableMetaData,
) -> jackcess::Result<()> {
    // the table object itself knows whether it is a type 4 definition (a
    // linked type 6 table resolves to the linkee's table, also this way)
    let (page, definition_only) = match db.find_table(&meta.name) {
        Ok(Some(table)) => (table.table_def_page_number, table.is_definition_only()),
        Ok(None) => return Ok(()),
        Err(_) => return Ok(()),
    };
    // Java opens the table and reads its first row; only a definition-only
    // (linked ODBC) table has no data access
    let error = if definition_only && db.tables.contains_key(&page) {
        match db.read_table_rows(page) {
            Ok(_) => "NONE".to_string(),
            Err(error) => format!("{}|{}", error_tag(&error), esc(&error_message(&error))),
        }
    } else {
        "NONE".to_string()
    };
    let _ = writeln!(out, "OPEN|{}|error={}", esc(&meta.name), error);
    Ok(())
}

// ---------------------------------------------------------------------------
// formatting helpers
// ---------------------------------------------------------------------------

/// The Java exception class the error corresponds to.
fn error_tag(error: &Error) -> &'static str {
    match error {
        Error::Io(_) | Error::Jackcess(_) => "IOException",
        Error::IllegalArgument(_) => "IllegalArgumentException",
        Error::IllegalState(_) => "IllegalStateException",
        Error::InvalidCredentials(_) => "InvalidCredentialsException",
        Error::Unsupported(_) => "UnsupportedOperationException",
        Error::ConstraintViolation(_) => "ConstraintViolationException",
        Error::InvalidValue(_) => "InvalidValueException",
        Error::BatchUpdate { .. } => "BatchUpdateException",
        Error::BufferUnderflow(_) | Error::BufferOverflow(_) => "RuntimeException",
        // java.nio's `InvalidMarkException` (corrupt null mask path)
        Error::InvalidMark => "InvalidMarkException",
        Error::NumberFormat(_) => "NumberFormatException",
    }
}

/// The Java `Throwable.getMessage()` of the Rust error.
fn error_message(error: &Error) -> String {
    match error {
        Error::Io(e) => e.to_string(),
        Error::Jackcess(message)
        | Error::IllegalArgument(message)
        | Error::IllegalState(message)
        | Error::InvalidCredentials(message)
        | Error::Unsupported(message)
        | Error::ConstraintViolation(message)
        | Error::InvalidValue(message)
        | Error::BufferUnderflow(message)
        | Error::BufferOverflow(message) => message.clone(),
        // `InvalidMarkException.getMessage()` is null
        Error::InvalidMark => "null".to_string(),
        Error::NumberFormat(message) => message.clone(),
        Error::BatchUpdate { message, .. } => message.clone(),
    }
}

/// A raw SQL value as a record field; `<null>` is SQL `NULL`.
fn value_field(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "<null>".to_string(),
        Some(Value::Text(text)) => esc(text),
        Some(Value::Bytes(bytes)) => hex(bytes),
        Some(Value::Bool(value)) => value.to_string(),
        // a Byte prints like Java's Byte.toString (`Attribute` 0xFF is -1)
        Some(Value::Byte(value)) => (*value as i8).to_string(),
        Some(Value::Int(value)) => value.to_string(),
        Some(Value::Long(value)) => value.to_string(),
        Some(Value::BigInt(value)) => value.to_string(),
        Some(Value::Float(value)) => value.to_string(),
        Some(Value::Double(value)) => value.to_string(),
        Some(Value::Decimal { unscaled, scale }) => format!("{unscaled}e-{scale}"),
        Some(Value::DateTime(date)) => format_date(Some(*date)),
        Some(Value::DateBits(bits)) => bits.to_string(),
        Some(Value::Uuid(bytes)) => hex(bytes),
        Some(Value::Keep) => "<keep>".to_string(),
    }
}

fn esc(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\t' => result.push_str("\\t"),
            '\r' => result.push_str("\\r"),
            '\n' => result.push_str("\\n"),
            '\0' => result.push_str("\\0"),
            other => result.push(other),
        }
    }
    result
}

fn esc_option(text: Option<&str>) -> String {
    match text {
        Some(text) => esc(text),
        None => "<null>".to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(result, "{byte:02x}");
    }
    result
}

/// `yyyy-MM-ddTHH:mm:ss.SSS` (the Java tool formats the same way), or `-` for
/// a missing date.
fn format_date(date: Option<jackcess::datenums::LocalDateTime>) -> String {
    match date {
        None => "-".to_string(),
        Some(date) => format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
            date.year,
            date.month,
            date.day,
            date.hour,
            date.minute,
            date.second,
            date.nano / 1_000_000
        ),
    }
}
