//! `objapi` - the Rust counterpart of `java-src/ObjApiGen.java`: dumps the
//! `Column`/`Index`/`Relationship`/`TableNameMap` object API surface in the
//! canonical text `tools/verify-objapi.ps1` compares against the Java
//! reference.
//!
//! Usage:
//! ```text
//! objapi dump <file.mdb|file.accdb>   # dump the accessors
//! objapi save <file.mdb|file.accdb>   # mutate + save the ApiNameMap name map
//! ```
//!
//! Every line is `key=value`; keys and string values are ASCII escaped
//! (`\uXXXX`) so the sorted lines of the two tools agree for non-ASCII names.
//! Values are type tagged (`str:`, `bool:`, `num:`, `null`,
//! `throws:<SimpleName>:<message>`).
//!
//! The documented mappings of the Java tool apply here: the owning
//! table/database is passed to the accessors Java answers from an object
//! reference (`Column::get_table`, `Column::get_database`,
//! `Column::get_version_history_column`, `Column::get_complex_info`,
//! `Index::get_table`, `Relationship::get_from_table`/`get_to_table`), and the
//! SQL type of a data type without one is reported by exception class only
//! (Java's message embeds an identity hash).  Linked tables need a
//! `LinkResolver`, so they (and the relationships of a database which holds
//! any) are reported as `skipped`.

use std::path::Path;

use jackcess::column::Column;
use jackcess::cursor::CursorBuilder;
use jackcess::database::{Database, TableMetaData, TableMetaDataType};
use jackcess::error::{Error, Result};
use jackcess::index::{IndexImpl, IndexImpl as Index};
use jackcess::namemap::{Mismatch, Record};
use jackcess::property::PropertyMap;
use jackcess::value::Value;

const NAME_MAP_TABLE: &str = "ApiNameMap";
const ADDED_NAME: &str = "ApiAdded";
const ADDED_GUID: &str = "{ABCDEF01-2345-6789-ABCD-EF0123456789}";
const REMOVED_NAME: &str = "ApiRemoved";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: objapi dump|save <database>");
        std::process::exit(2);
    }
    let path = Path::new(&args[2]);
    let result = match args[1].as_str() {
        "dump" => run_dump(path),
        "save" => run_save(path),
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    };
    match result {
        Ok(lines) => {
            let mut out = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
            let mut lines = lines;
            lines.sort();
            for line in lines {
                out.push_str(&line);
                out.push('\n');
            }
            print!("{out}");
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn run_dump(path: &Path) -> Result<Vec<String>> {
    let mut db = Database::open_path(path)?;
    let mut out = Vec::new();
    dump_impl(&mut db, &mut out)?;
    Ok(out)
}

fn dump_impl(db: &mut Database, out: &mut Vec<String>) -> Result<()> {
    let db_name = db.get_name().to_string();
    put(out, "db.name", str_value(&db_name));

    let mut names = db.get_table_names()?;
    names.sort();
    let mut linked = false;
    for name in &names {
        if is_linked(db, name)? {
            linked = true;
        }
    }
    for name in &names {
        let prefix = format!("table.{}.", escape(name));
        if is_linked(db, name)? {
            put(out, &(prefix + "skipped"), str_value("linked"));
            continue;
        }
        match db.get_table(name) {
            Ok(table) => {
                let _ = table;
            }
            Err(e) => {
                put(out, &(prefix + "error"), exc(&e));
                continue;
            }
        }
        dump_table(db, name, out)?;
        dump_name_map(db, name, out)?;
    }
    dump_relationships(db, &names, linked, out)?;
    Ok(())
}

fn is_linked(db: &mut Database, name: &str) -> Result<bool> {
    Ok(matches!(
        db.get_table_meta_data(name)?
            .map(|meta: TableMetaData| meta.table_type),
        Some(TableMetaDataType::Linked)
    ))
}

fn dump_table(db: &mut Database, table_name: &str, out: &mut Vec<String>) -> Result<()> {
    let table_prefix = format!("table.{}.", escape(table_name));
    let columns: Vec<Column>;
    let indexes: Vec<IndexImpl>;
    let mut builders: Vec<CursorBuilder>;
    {
        let table = db.get_table(table_name)?;
        put(
            out,
            &(table_prefix.clone() + "name"),
            str_value(table.get_name()),
        );
        put(
            out,
            &(table_prefix.clone() + "column_count"),
            num_value(table.get_column_count() as i64),
        );
        columns = table.get_columns().iter().map(|c| (*c).clone()).collect();
        indexes = table.indexes.clone();
        // the accessors which take the owning table
        for column in &columns {
            let p = format!("column.{}.{}.", escape(table_name), escape(&column.name));
            let value = match column.get_table(table) {
                Ok(owner) => str_value(owner.get_name()),
                Err(e) => exc(&e),
            };
            put(out, &(p + "table"), value);
        }
        builders = Vec::with_capacity(indexes.len());
        for index in &indexes {
            let p = format!("index.{}.{}.", escape(table_name), escape(&index.name));
            let value = match index.get_table(table) {
                Ok(owner) => str_value(owner.get_name()),
                Err(e) => exc(&e),
            };
            put(out, &(p.clone() + "table"), value);
            put(
                out,
                &(p.clone() + "column_count"),
                num_value(index.get_column_count(table) as i64),
            );
            put(
                out,
                &(p + "columns"),
                str_value(&join_index_columns(&index.get_columns(table))),
            );
            builders.push(index.new_cursor(table));
        }
    }

    let format = db.format.clone();
    let page = table_page(db, table_name)?;
    for column in &columns {
        let p = format!("column.{}.{}.", escape(table_name), escape(&column.name));
        put(
            out,
            &(p.clone() + "database"),
            str_value(column.get_database(db).get_name()),
        );
        put(out, &(p.clone() + "sql_type"), sql_type(column));
        put(
            out,
            &(p.clone() + "length_in_units"),
            num_value(column.get_length_in_units(&format) as i64),
        );
        put(
            out,
            &(p.clone() + "hyperlink"),
            bool_value(column.is_hyperlink()),
        );
        put(
            out,
            &(p.clone() + "security_identifier"),
            bool_value(column.is_security_identifier()),
        );
        put(
            out,
            &(p.clone() + "column_index"),
            num_value(column.get_column_index() as i64),
        );
        put(
            out,
            &(p.clone() + "column_id"),
            num_value(column.get_column_id() as i64),
        );
        let value = match column.get_version_history_column(db, table_name) {
            Ok(Some(version)) => str_value(&version.name),
            Ok(None) => "null".to_string(),
            Err(e) => exc(&e),
        };
        put(out, &(p.clone() + "version_history_column"), value);
        let value = match column.get_complex_info(db, table_name) {
            Ok(Some(info)) => str_value(complex_type_name(info.kind)),
            Ok(None) => "null".to_string(),
            Err(e) => exc(&e),
        };
        put(out, &(p.clone() + "complex_info"), value);
        match column.get_properties(db, page) {
            Ok(map) => dump_properties(&format!("{p}properties."), &map, out),
            Err(e) => put(out, &(p + "properties.read"), exc(&e)),
        }
    }

    for (index, mut builder) in indexes.iter().zip(builders.drain(..)) {
        let p = format!("index.{}.{}.", escape(table_name), escape(&index.name));
        put(
            out,
            &(p.clone() + "referenced_index"),
            referenced_index(db, table_name, index),
        );
        let value = match builder.to_index_cursor(db) {
            Ok(cursor) => str_value(cursor.table_name()),
            Err(e) => exc(&e),
        };
        put(out, &(p.clone() + "new_cursor.table"), value);
        match index.get_properties(db, page) {
            Ok(map) => dump_properties(&format!("{p}properties."), &map, out),
            Err(e) => put(out, &(p + "properties.read"), exc(&e)),
        }
    }
    Ok(())
}

fn table_page(db: &mut Database, table_name: &str) -> Result<i32> {
    Ok(db.get_table(table_name)?.table_def_page_number)
}

fn join_index_columns(columns: &[jackcess::index::IndexColumn]) -> String {
    columns
        .iter()
        .map(|column| {
            format!(
                "{}|{}|{}|{}",
                escape(column.get_name()),
                escape(&column.get_column().name),
                column.get_column_index(),
                if column.is_ascending() { "asc" } else { "desc" }
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// The Java `ComplexDataType` constant name (`ComplexDataType.name()`).
fn complex_type_name(kind: jackcess::complex::ComplexDataType) -> &'static str {
    use jackcess::complex::ComplexDataType;
    match kind {
        ComplexDataType::Attachment => "ATTACHMENT",
        ComplexDataType::MultiValue => "MULTI_VALUE",
        ComplexDataType::VersionHistory => "VERSION_HISTORY",
        ComplexDataType::Unsupported => "UNSUPPORTED",
    }
}

fn sql_type(column: &Column) -> String {
    match column.get_sql_type() {
        Ok(sql_type) => num_value(sql_type as i64),
        // `DataType.getSQLType` throws a `JackcessException` whose message
        // embeds `DataType`'s identity-hash `toString`, which cannot be
        // reproduced; the class name is the comparable part
        Err(_) => "throws:JackcessException".to_string(),
    }
}

fn referenced_index(db: &mut Database, table_name: &str, index: &Index) -> String {
    let reference_page = index
        .reference
        .map(|reference| reference.other_table_page_number);
    match index.get_referenced_index(db, table_name) {
        Ok(None) => str_value("-"),
        Ok(Some(referenced)) => {
            let table = reference_page
                .and_then(|page| db.get_table_by_page(page).ok())
                .map(|table| table.name.clone())
                .unwrap_or_default();
            str_value(&format!("{}.{}", table, referenced.name))
        }
        Err(e) => exc(&e),
    }
}

fn dump_relationships(
    db: &mut Database,
    _names: &[String],
    linked: bool,
    out: &mut Vec<String>,
) -> Result<()> {
    if linked {
        // a linked table needs a link resolver (and its linkee file) to open,
        // and the relationship accessors resolve their tables
        put(out, "rel", str_value("skipped:linked"));
        return Ok(());
    }
    let relationships = match db.relationships_all() {
        Ok(relationships) => relationships,
        Err(e) => {
            put(out, "rel.error", exc(&e));
            return Ok(());
        }
    };
    for relationship in &relationships {
        let p = format!("rel.{}.", escape(&relationship.name));
        put(
            out,
            &(p.clone() + "from_table"),
            str_value(relationship.get_from_table(db)?.get_name()),
        );
        put(
            out,
            &(p.clone() + "to_table"),
            str_value(relationship.get_to_table(db)?.get_name()),
        );
        let from_columns = relationship.get_from_columns(db)?;
        put(
            out,
            &(p.clone() + "from_columns"),
            str_value(&join_columns(&from_columns)),
        );
        let to_columns = relationship.get_to_columns(db)?;
        put(
            out,
            &(p.clone() + "to_columns"),
            str_value(&join_columns(&to_columns)),
        );
        put(
            out,
            &(p.clone() + "join_type"),
            str_value(relationship.join_type().name()),
        );
        put(
            out,
            &(p.clone() + "one_to_one"),
            bool_value(relationship.is_one_to_one()),
        );
        put(
            out,
            &(p.clone() + "referential_integrity"),
            bool_value(relationship.has_referential_integrity()),
        );
        put(
            out,
            &(p.clone() + "cascade_updates"),
            bool_value(relationship.cascade_updates()),
        );
        put(
            out,
            &(p.clone() + "cascade_deletes"),
            bool_value(relationship.cascade_deletes()),
        );
        put(
            out,
            &(p.clone() + "cascade_null_on_delete"),
            bool_value(relationship.cascade_null_on_delete()),
        );
        put(
            out,
            &(p.clone() + "left_outer"),
            bool_value(relationship.is_left_outer_join()),
        );
        put(
            out,
            &(p.clone() + "right_outer"),
            bool_value(relationship.is_right_outer_join()),
        );
    }
    Ok(())
}

fn join_columns(columns: &[Column]) -> String {
    columns
        .iter()
        .map(|column| escape(&column.name))
        .collect::<Vec<_>>()
        .join(",")
}

fn dump_name_map(db: &mut Database, table_name: &str, out: &mut Vec<String>) -> Result<()> {
    let p = format!("map.{}.", escape(table_name));
    let page = table_page(db, table_name)?;
    let mut map = match db.get_table_name_map_by_page(page) {
        Ok(Some(map)) => map,
        Ok(None) => {
            put(out, &(p + "state"), str_value("absent"));
            return Ok(());
        }
        Err(e) => {
            put(out, &(p + "state"), exc(&e));
            return Ok(());
        }
    };
    put(out, &(p.clone() + "state"), str_value("present"));
    let properties = db.get_table_properties(page)?;
    let blob = properties
        .default_map()
        .and_then(|properties| properties.get_value(jackcess::property::NAME_MAP_PROP));
    put(
        out,
        &(p.clone() + "blob"),
        match blob {
            Some(Value::Bytes(bytes)) => hex(bytes),
            _ => "-".to_string(),
        },
    );
    put(
        out,
        &(p.clone() + "records"),
        num_value(map.records().len() as i64),
    );
    for (index, record) in map.records().iter().enumerate() {
        put(out, &format!("{p}r{index}"), fmt_record(record));
    }
    put(
        out,
        &(p.clone() + "table_record"),
        fmt_or_dash(map.get_table_record()),
    );
    put(
        out,
        &(p.clone() + "column_records"),
        num_value(map.get_column_records().len() as i64),
    );
    put(
        out,
        &(p.clone() + "references"),
        num_value(map.get_references().len() as i64),
    );
    put(
        out,
        &(p.clone() + "writer_tag"),
        match map.get_writer_tag() {
            Some(tag) => num_value(tag as i64),
            None => "null".to_string(),
        },
    );

    // the mutating API on the in-memory map only (not saved here)
    match map.add_column(ADDED_NAME, ADDED_GUID) {
        Ok(record) => {
            put(out, &(p.clone() + "add.record"), fmt_record(&record));
            put(out, &(p.clone() + "add.blob"), hex(&map.to_bytes()));
        }
        Err(e) => put(out, &(p.clone() + "add.record"), exc(&e)),
    }
    let removed = map.remove_column(REMOVED_NAME);
    put(
        out,
        &(p.clone() + "remove.record"),
        fmt_or_dash(removed.as_ref()),
    );
    put(out, &(p.clone() + "after.blob"), hex(&map.to_bytes()));
    put(
        out,
        &(p.clone() + "after.records"),
        num_value(map.records().len() as i64),
    );
    let properties = db.get_table_properties(page)?;
    let table = db.get_table(table_name)?;
    match map.find_mismatches(table, &properties) {
        Ok(mismatches) => {
            put(
                out,
                &(p.clone() + "mismatches"),
                num_value(mismatches.len() as i64),
            );
            for (index, mismatch) in mismatches.iter().enumerate() {
                put(out, &format!("{p}m{index}"), fmt_mismatch(mismatch));
            }
        }
        Err(e) => put(out, &(p.clone() + "mismatches"), exc(&e)),
    }
    Ok(())
}

fn run_save(path: &Path) -> Result<Vec<String>> {
    let mut db = Database::open_path(path)?;
    let mut out = Vec::new();
    let page = match db.get_table(NAME_MAP_TABLE) {
        Ok(table) => table.table_def_page_number,
        Err(_) => {
            put(&mut out, "save.state", str_value("absent"));
            return Ok(out);
        }
    };
    let mut map = match db.get_table_name_map_by_page(page)? {
        Some(map) => map,
        None => {
            put(&mut out, "save.state", str_value("absent"));
            return Ok(out);
        }
    };
    let added = map.add_column(ADDED_NAME, ADDED_GUID)?;
    put(&mut out, "save.added", fmt_record(&added));
    let removed = map.remove_column(REMOVED_NAME);
    put(&mut out, "save.removed", fmt_or_dash(removed.as_ref()));
    map.save(&mut db, page)?;
    db.flush()?;

    put(&mut out, "save.state", str_value("saved"));
    let expected = map.to_bytes();
    let stored = db
        .get_table_properties(page)?
        .default_map()
        .and_then(|properties| properties.get_value(jackcess::property::NAME_MAP_PROP))
        .cloned();
    put(
        &mut out,
        "save.blob",
        match &stored {
            Some(Value::Bytes(bytes)) => hex(bytes),
            _ => "-".to_string(),
        },
    );
    put(&mut out, "save.blob_expected", hex(&expected));
    put(
        &mut out,
        "save.records",
        num_value(map.records().len() as i64),
    );
    for (index, record) in map.records().iter().enumerate() {
        put(&mut out, &format!("save.r{index}"), fmt_record(record));
    }
    let properties = db.get_table_properties(page)?;
    let table = db.get_table(NAME_MAP_TABLE)?;
    match map.find_mismatches(table, &properties) {
        Ok(mismatches) => {
            put(
                &mut out,
                "save.mismatches",
                num_value(mismatches.len() as i64),
            );
            for (index, mismatch) in mismatches.iter().enumerate() {
                put(&mut out, &format!("save.m{index}"), fmt_mismatch(mismatch));
            }
        }
        Err(e) => put(&mut out, "save.mismatches", exc(&e)),
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// formatting
// ---------------------------------------------------------------------------

fn fmt_record(record: &Record) -> String {
    format!(
        "type={} guid={} parent={} name={} date={}",
        record.get_type().name(),
        record.get_guid().unwrap_or_else(|| "-".to_string()),
        record.get_parent_guid().unwrap_or_else(|| "-".to_string()),
        escape(record.get_name()),
        record
            .get_date()
            .map(|date| date.to_epoch_millis().to_string())
            .unwrap_or_else(|| "-".to_string())
    )
}

fn fmt_or_dash(record: Option<&Record>) -> String {
    record.map(fmt_record).unwrap_or_else(|| "-".to_string())
}

fn fmt_mismatch(mismatch: &Mismatch) -> String {
    format!(
        "type={} table={} query={} column={} record={}",
        mismatch.get_type().name(),
        mismatch
            .get_table()
            .map(escape)
            .unwrap_or_else(|| "-".to_string()),
        mismatch
            .get_query()
            .map(escape)
            .unwrap_or_else(|| "-".to_string()),
        mismatch
            .get_column()
            .map(escape)
            .unwrap_or_else(|| "-".to_string()),
        fmt_or_dash(mismatch.get_record())
    )
}

fn dump_properties(prefix: &str, map: &PropertyMap, out: &mut Vec<String>) {
    let mut names: Vec<&str> = map.names().collect();
    names.sort();
    put(
        out,
        &(prefix.to_string() + "count"),
        num_value(names.len() as i64),
    );
    for name in names {
        put(
            out,
            &(prefix.to_string() + &escape(name)),
            property_value(map.get_value(name)),
        );
    }
}

fn property_value(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "null".to_string(),
        Some(Value::Bool(value)) => bool_value(*value),
        Some(Value::Byte(value)) => num_value((*value as i8) as i64),
        Some(Value::Int(_)) | Some(Value::Long(_)) | Some(Value::BigInt(_)) => {
            num_value(value.unwrap().as_i64().unwrap_or(0))
        }
        Some(Value::Float(v)) => format!("num:{v:?}"),
        Some(Value::Double(v)) => format!("num:{v:?}"),
        Some(Value::Decimal { unscaled, scale }) => {
            format!("num:{}", decimal_to_string(*unscaled, *scale))
        }
        Some(Value::Bytes(bytes)) => format!("bytes:{}", hex(bytes)),
        Some(Value::Text(text)) => str_value(text),
        Some(Value::DateTime(date)) => num_value(date.to_epoch_millis()),
        Some(Value::DateBits(bits)) => str_value(&format!("datebits:{bits}")),
        Some(Value::Uuid(bytes)) => format!("bytes:{}", hex(bytes)),
        Some(Value::Keep) => str_value("KEEP_VALUE"),
    }
}

fn decimal_to_string(unscaled: i128, scale: i32) -> String {
    // `java.math.BigDecimal.toString`: a plain decimal for a non-negative
    // scale (keeping trailing zeros), scientific notation for a negative one
    let negative = unscaled < 0;
    let digits = unscaled.unsigned_abs().to_string();
    let body = if scale == 0 {
        digits
    } else if scale < 0 {
        format!("{}E+{}", digits, -(scale as i64))
    } else {
        let scale = scale as usize;
        if digits.len() > scale {
            let point = digits.len() - scale;
            format!("{}.{}", &digits[..point], &digits[point..])
        } else {
            format!("0.{}{}", "0".repeat(scale - digits.len()), digits)
        }
    };
    if negative {
        format!("-{body}")
    } else {
        body
    }
}

fn exc(error: &Error) -> String {
    let (class, message) = match error {
        Error::Io(e) => ("IOException", e.to_string()),
        Error::Jackcess(message) => ("IOException", message.clone()),
        Error::IllegalArgument(message) => ("IllegalArgumentException", message.clone()),
        Error::IllegalState(message) => ("IllegalStateException", message.clone()),
        Error::InvalidCredentials(message) => ("InvalidCredentialsException", message.clone()),
        Error::Unsupported(message) => ("UnsupportedOperationException", message.clone()),
        Error::ConstraintViolation(message) => ("ConstraintViolationException", message.clone()),
        Error::InvalidValue(message) => ("InvalidValueException", message.clone()),
        Error::BatchUpdate { message, .. } => ("BatchUpdateException", message.clone()),
        Error::BufferUnderflow(message) => ("BufferUnderflowException", message.clone()),
        Error::BufferOverflow(message) => ("BufferOverflowException", message.clone()),
        Error::InvalidMark => ("InvalidMarkException", String::new()),
        Error::NumberFormat(message) => ("NumberFormatException", message.clone()),
    };
    format!("throws:{}:{}", class, escape(&message))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Printable ASCII stays, every other UTF-16 unit becomes `\uXXXX` (the same
/// escaping as `ObjApiGen.esc`), so the two sorted dumps compare exactly.
fn escape(value: &str) -> String {
    let mut out = String::new();
    for unit in value.encode_utf16() {
        if (0x20..=0x7E).contains(&unit) && unit != '\\' as u16 {
            out.push(char::from_u32(unit as u32).unwrap());
        } else {
            out.push_str(&format!("\\u{unit:04X}"));
        }
    }
    out
}

fn str_value(value: &str) -> String {
    format!("str:{}", escape(value))
}

fn bool_value(value: bool) -> String {
    format!("bool:{value}")
}

fn num_value(value: i64) -> String {
    format!("num:{value}")
}

fn put(out: &mut Vec<String>, key: &str, value: String) {
    out.push(format!("{key}={value}"));
}
