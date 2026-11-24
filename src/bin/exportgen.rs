//! `exportgen` - generates databases and exports/imports delimited text with
//! the Rust port, mirroring `java-src/ExportGen.java`.
//!
//! Modes:
//! * `gen <db> <format> [--now-ms N]` - the export source database.
//! * `export <db> <out.txt> <mode>` - export the source table.
//! * `import <db> <format> <fixture> <mode> [--now-ms N]` - import a fixture.

use jackcess::column::ColumnSpec;
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::datenums::LocalDateTime;
use jackcess::error::{Error, Result};
use jackcess::export::{
    export_table_to_file, import_file, ExportColumn, ExportFilter, ExportOptions, ImportColumn,
    ImportFilter, ImportOptions,
};
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

const SOURCE_TABLE: &str = "ExportData";
const IMPORT_TABLE: &str = "Imported";
const EXISTING_TABLE: &str = "Target";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let mut now_ms: Option<i64> = None;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--now-ms" {
            now_ms = args.get(i + 1).and_then(|s| s.parse::<i64>().ok());
            i += 2;
        } else {
            positional.push(args[i].clone());
            i += 1;
        }
    }

    let result = match positional.first().map(|s| s.as_str()) {
        Some("gen") if positional.len() >= 3 => run_gen(&positional[1], &positional[2], now_ms),
        Some("export") if positional.len() >= 4 => {
            run_export(&positional[1], &positional[2], &positional[3])
        }
        Some("import") if positional.len() >= 5 => run_import(
            &positional[1],
            &positional[2],
            &positional[3],
            &positional[4],
            now_ms,
        ),
        _ => {
            usage();
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn usage() {
    eprintln!(
        "usage: exportgen gen <db> <format> [--now-ms N] | \
         export <db> <out.txt> <mode> | import <db> <format> <fixture> <mode> [--now-ms N]"
    );
}

fn parse_format(name: &str) -> Result<FileFormat> {
    FileFormat::from_name(name)
        .ok_or_else(|| Error::IllegalArgument(format!("unknown format {name}")))
}

// ---------------------------------------------------------------------------
// export source database (mirrors ExportGen.makeExportTable/runGen)
// ---------------------------------------------------------------------------

fn source_spec() -> TableSpec {
    TableSpec::new(
        SOURCE_TABLE,
        vec![
            ColumnSpec::new("Num", DataType::Long),
            ColumnSpec::new("Label", DataType::Text).with_length(50),
            ColumnSpec::new("Qty", DataType::Int),
            ColumnSpec::new("Price", DataType::Money),
            ColumnSpec::new("Ratio", DataType::Double),
            ColumnSpec::new("Score", DataType::Float),
            ColumnSpec::new("Small", DataType::Byte),
            ColumnSpec::new("Flag", DataType::Boolean),
            ColumnSpec::new("Stamp", DataType::ShortDateTime),
            ColumnSpec::new("Raw", DataType::Binary).with_length(30),
            ColumnSpec::new("Token", DataType::Guid),
            ColumnSpec::new("Note", DataType::Memo),
            ColumnSpec::new("Empty", DataType::Text).with_length(20),
        ],
    )
}

fn money(unscaled: i128) -> Value {
    Value::Decimal { unscaled, scale: 4 }
}

fn source_rows() -> Vec<Vec<Value>> {
    let rows: Vec<Vec<Value>> = vec![
        vec![
            Value::Long(1),
            Value::Text("plain".into()),
            Value::Int(7),
            money(123400),
            Value::Double(1.5),
            Value::Float(2.5),
            Value::Long(100),
            Value::Bool(true),
            Value::DateTime(LocalDateTime::new(2020, 3, 15, 12, 30, 45, 0)),
            Value::Bytes(vec![1, 2, 3]),
            Value::Text("{0E984725-C51C-4BF4-9960-E1C80E27ABA0}".into()),
            Value::Text("memo one".into()),
            Value::Text(String::new()),
        ],
        vec![
            Value::Long(2),
            Value::Text("comma,here".into()),
            Value::Int(-3),
            money(-55000),
            Value::Double(-2.25),
            Value::Float(-0.125),
            Value::Long(-1),
            Value::Bool(false),
            Value::DateTime(LocalDateTime::new(2021, 1, 2, 3, 4, 5, 0)),
            Value::Bytes(vec![0, 255, 16, 32, 48, 64, 128]),
            Value::Null,
            Value::Text("quote \" inside".into()),
            Value::Null,
        ],
        vec![
            Value::Long(3),
            Value::Text("quote\"it's inside".into()),
            Value::Int(0),
            money(0),
            Value::Double(0.001),
            Value::Float(0.001),
            Value::Long(-56),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Text("{89ABCDEF-0123-4567-89AB-CDEF01234567}".into()),
            Value::Text("line1\nline2".into()),
            Value::Text(" ".into()),
        ],
        vec![
            Value::Null,
            Value::Text("cr\rhere".into()),
            Value::Null,
            Value::Null,
            Value::Double(1.0e7),
            Value::Float(1.0e7),
            Value::Null,
            Value::Bool(true),
            Value::DateTime(LocalDateTime::new(2022, 6, 7, 8, 9, 10, 0)),
            Value::Bytes((0u8..30).collect()),
            Value::Null,
            Value::Null,
            Value::Text("x".into()),
        ],
        vec![
            Value::Long(-9),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Double(1.0e-4),
            Value::Float(1.0e-4),
            Value::Long(0),
            Value::Bool(false),
            Value::DateTime(LocalDateTime::new(1999, 12, 31, 23, 59, 59, 0)),
            Value::Bytes(Vec::new()),
            Value::Null,
            Value::Text(" tab\tand semicolon; ".into()),
            Value::Null,
        ],
        vec![
            Value::Long(42),
            Value::Text(String::new()),
            Value::Int(1),
            money(1234567891),
            Value::Double(-0.0),
            Value::Float(-0.0),
            Value::Long(127),
            Value::Bool(false),
            Value::DateTime(LocalDateTime::new(2000, 2, 29, 0, 0, 0, 0)),
            Value::Bytes(vec![0xFF; 30]),
            Value::Null,
            Value::Text("multi\r\nline".into()),
            Value::Text("end".into()),
        ],
        vec![
            Value::Long(7),
            Value::Text("semi;colon".into()),
            Value::Int(2),
            money(-1),
            Value::Double(1234567.0),
            Value::Float(1234.5),
            Value::Long(-128),
            Value::Bool(true),
            Value::DateTime(LocalDateTime::new(2001, 9, 9, 1, 2, 3, 0)),
            Value::Bytes(vec![1, 2, 3, 4, 5]),
            Value::Null,
            Value::Text(String::new()),
            Value::Null,
        ],
        vec![
            Value::Long(8),
            Value::Text("it's".into()),
            Value::Int(3),
            money(15000),
            Value::Double(-1.25),
            Value::Float(1234567.0),
            Value::Null,
            Value::Bool(true),
            Value::DateTime(LocalDateTime::new(2023, 12, 31, 23, 59, 59, 0)),
            Value::Bytes(vec![9]),
            Value::Null,
            Value::Text("tail".into()),
            Value::Null,
        ],
    ];
    rows
}

fn run_gen(path: &str, format: &str, now_ms: Option<i64>) -> Result<()> {
    let format = parse_format(format)?;
    let file = std::path::Path::new(path);
    if file.exists() {
        std::fs::remove_file(file)?;
    }
    let mut db = Database::create(format, file)?;
    db.now_ms = now_ms;
    db.create_table(&source_spec())?;
    for row in source_rows() {
        db.add_rows(SOURCE_TABLE, &[row])?;
    }
    db.flush()?;
    println!("wrote {path}");
    Ok(())
}

// ---------------------------------------------------------------------------
// export (mirrors ExportGen.SourceFilter and the export modes)
// ---------------------------------------------------------------------------

struct SourceFilter;

impl ExportFilter for SourceFilter {
    fn filter_columns(&self, columns: &[ExportColumn]) -> Vec<ExportColumn> {
        columns
            .iter()
            .filter(|c| c.name != "Note")
            .cloned()
            .collect()
    }

    fn filter_row(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
        if row.first().map(Value::is_null).unwrap_or(true) {
            return Ok(None);
        }
        let mut row = row.to_vec();
        if matches!(row.first(), Some(Value::Long(7))) {
            if let Some(Value::Text(label)) = row.get_mut(1) {
                *label = format!("[{label}]");
            }
        }
        Ok(Some(row))
    }
}

fn run_export(db_path: &str, out_path: &str, mode: &str) -> Result<()> {
    let mut db = Database::open_path(std::path::Path::new(db_path))?;
    let mut opts = ExportOptions::default();
    match mode {
        "default" => {}
        "header" => opts = opts.with_header(true),
        "semicolon" => opts = opts.with_header(true).with_delimiter(";").with_quote('\''),
        "tab" => opts = opts.with_header(true).with_delimiter("\t"),
        "filter" => opts = opts.with_header(true).with_filter(Box::new(SourceFilter)),
        other => {
            return Err(Error::IllegalArgument(format!(
                "unknown export mode {other}"
            )))
        }
    }
    export_table_to_file(&mut db, SOURCE_TABLE, std::path::Path::new(out_path), &opts)?;
    println!("exported {out_path}");
    Ok(())
}

// ---------------------------------------------------------------------------
// import (mirrors ExportGen.TypedFilter and the import modes)
// ---------------------------------------------------------------------------

struct TypedFilter;

impl ImportFilter for TypedFilter {
    fn filter_row(&self, row: &[Value]) -> Result<Option<Vec<Value>>> {
        let mut row = row.to_vec();
        for (i, value) in row.iter_mut().enumerate() {
            if let Value::Text(text) = value {
                if i == 5 {
                    *value = if text.is_empty() {
                        Value::Null
                    } else {
                        Value::DateTime(parse_iso_local_date_time(text)?)
                    };
                } else if i != 1 && i != 7 && text.is_empty() {
                    // numeric / boolean empty fields are nulls; Label and
                    // Remark keep their zero length strings
                    *value = Value::Null;
                }
            }
        }
        Ok(Some(row))
    }
}

fn import_types() -> Vec<ImportColumn> {
    vec![
        ImportColumn::new(DataType::Long),
        ImportColumn::new(DataType::Text).with_length(40),
        ImportColumn::new(DataType::Int),
        ImportColumn::new(DataType::Money),
        ImportColumn::new(DataType::Boolean),
        ImportColumn::new(DataType::ShortDateTime),
        ImportColumn::new(DataType::Double),
        ImportColumn::new(DataType::Memo),
        ImportColumn::new(DataType::Byte),
    ]
}

fn typed_table_spec(name: &str) -> TableSpec {
    TableSpec::new(
        name,
        vec![
            ColumnSpec::new("Num", DataType::Long),
            ColumnSpec::new("Label", DataType::Text).with_length(40),
            ColumnSpec::new("Qty", DataType::Int),
            ColumnSpec::new("Price", DataType::Money),
            ColumnSpec::new("Flag", DataType::Boolean),
            ColumnSpec::new("Stamp", DataType::ShortDateTime),
            ColumnSpec::new("Ratio", DataType::Double),
            ColumnSpec::new("Remark", DataType::Memo),
            ColumnSpec::new("Tiny", DataType::Byte),
        ],
    )
}

fn run_import(
    db_path: &str,
    format: &str,
    fixture: &str,
    mode: &str,
    now_ms: Option<i64>,
) -> Result<()> {
    let format = parse_format(format)?;
    let path = std::path::Path::new(db_path);
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(format, path)?;
    db.now_ms = now_ms;

    let opts;
    match mode {
        "new" | "crlf" => {
            opts = ImportOptions::new(IMPORT_TABLE)
                .with_column_types(import_types())
                .with_filter(Box::new(TypedFilter));
        }
        "semicolon" => {
            opts = ImportOptions::new(IMPORT_TABLE)
                .with_column_types(import_types())
                .with_filter(Box::new(TypedFilter))
                .with_delimiter(";")
                .with_quote('\'');
        }
        "tab" => {
            opts = ImportOptions::new(IMPORT_TABLE)
                .with_column_types(import_types())
                .with_filter(Box::new(TypedFilter))
                .with_delimiter("\t");
        }
        "existing" | "existing_noheader" => {
            db.create_table(&typed_table_spec(EXISTING_TABLE))?;
            opts = ImportOptions::new(EXISTING_TABLE)
                .with_use_existing_table(true)
                .with_header(mode == "existing")
                .with_filter(Box::new(TypedFilter));
        }
        "indexed" => {
            // an indexed target table exercises the import's 200 row batches:
            // the index pages are written/split like jackcess does
            let spec = typed_table_spec(EXISTING_TABLE).with_index(IndexSpec::primary_key(
                "PrimaryKey",
                vec![("Num".to_string(), true)],
            ));
            db.create_table(&spec)?;
            opts = ImportOptions::new(EXISTING_TABLE)
                .with_use_existing_table(true)
                .with_header(true)
                .with_filter(Box::new(TypedFilter));
        }
        other => {
            return Err(Error::IllegalArgument(format!(
                "unknown import mode {other}"
            )))
        }
    }

    let name = import_file(std::path::Path::new(fixture), &mut db, &opts)?
        .unwrap_or_else(|| "<none>".to_string());
    db.flush()?;
    println!("imported {name}");
    Ok(())
}

/// `LocalDateTime.parse` for the ISO representation used by the fixtures and
/// the export (`yyyy-MM-ddTHH:mm[:ss[.fff|.ffffff|.fffffffff]]`).
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
            let value: u32 = fraction
                .parse()
                .map_err(|_| Error::InvalidValue(format!("invalid date '{text}'")))?;
            nano = match fraction.len() {
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
