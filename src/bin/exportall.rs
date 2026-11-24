//! `exportall` - mirrors `java-src/ExportAllGen.java` for the
//! `ExportUtil.exportAll` port.
//!
//! Modes:
//! * `gen <db> <format>` - the multi table database.
//! * `genempty <db> <format>` - a database without user tables.
//! * `genhostile <db> <format>` - Alpha, Questions?Mark, Zulu.
//! * `exportall <db> <dir> <mode> <manifest>` - the directory export.
//! * `map <db> <dir> <mode> <manifest>` - the same export through
//!   [`jackcess::export::export_all_to_map`], written to files (no Java
//!   equivalent; compared against Java's directory output).
//! * `append <db> <file> <table>` - prefixes a file, then appends an
//!   `export_table_to_writer` export through an append writer.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use jackcess::column::ColumnSpec;
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::datenums::LocalDateTime;
use jackcess::error::{Error, Result};
use jackcess::export::{
    export_all, export_all_ext, export_all_to_map, export_all_with, export_file_name,
    export_table_to_writer, ExportColumn, ExportFilter, ExportOptions,
};
use jackcess::format::FileFormat;
use jackcess::tablecreator::TableSpec;
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let result = match args[1].as_str() {
        "gen" if args.len() == 4 => run_gen(Path::new(&args[2]), &args[3]),
        "genempty" if args.len() == 4 => run_empty(Path::new(&args[2]), &args[3]),
        "genhostile" if args.len() == 4 => run_hostile(Path::new(&args[2]), &args[3]),
        "exportall" if args.len() == 6 => run_export_all(
            Path::new(&args[2]),
            Path::new(&args[3]),
            &args[4],
            Path::new(&args[5]),
            false,
        ),
        "map" if args.len() == 6 => run_export_all(
            Path::new(&args[2]),
            Path::new(&args[3]),
            &args[4],
            Path::new(&args[5]),
            true,
        ),
        "append" if args.len() == 5 => {
            run_append(Path::new(&args[2]), Path::new(&args[3]), &args[4])
        }
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
        "usage: exportall gen|genempty|genhostile <db> <format> | \
         exportall|map <db> <dir> <mode> <manifest> | append <db> <file> <table>"
    );
}

fn parse_format(name: &str) -> Result<FileFormat> {
    FileFormat::from_name(name)
        .ok_or_else(|| Error::IllegalArgument(format!("unknown format {name}")))
}

// ---------------------------------------------------------------------------
// databases (mirror ExportAllGen)
// ---------------------------------------------------------------------------

fn money(unscaled: i128) -> Value {
    Value::Decimal { unscaled, scale: 4 }
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn bytes(values: &[u8]) -> Value {
    Value::Bytes(values.to_vec())
}

fn date(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> Value {
    Value::DateTime(LocalDateTime::new(
        year, month, day, hour, minute, second, 0,
    ))
}

fn create(path: &Path, fmt: &str) -> Result<Database> {
    let format = parse_format(fmt)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Database::create(format, path)
}

fn run_gen(path: &Path, fmt: &str) -> Result<()> {
    let mut db = create(path, fmt)?;

    db.create_table(&TableSpec::new(
        "Customers",
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
    ))?;
    let customers = vec![
        vec![
            Value::Long(1),
            text("plain"),
            Value::Long(7),
            money(123400),
            Value::Double(1.5),
            Value::Float(2.5),
            Value::Long(100),
            Value::Bool(true),
            date(2020, 3, 15, 12, 30, 45),
            bytes(&[1, 2, 3]),
            text("{0E984725-C51C-4BF4-9960-E1C80E27ABA0}"),
            text("memo one"),
            text(""),
        ],
        vec![
            Value::Long(2),
            text("comma,here"),
            Value::Long(-3),
            money(-55000),
            Value::Double(-2.25),
            Value::Float(-0.125),
            Value::Long(-1),
            Value::Bool(false),
            date(2021, 1, 2, 3, 4, 5),
            bytes(&[0, 255, 16, 32, 48, 64, 128]),
            Value::Null,
            text("quote \" inside"),
            Value::Null,
        ],
        vec![
            Value::Long(3),
            text("quote\"it's inside"),
            Value::Long(0),
            money(0),
            Value::Double(0.001),
            Value::Float(0.001),
            Value::Long(-56),
            Value::Null,
            Value::Null,
            Value::Null,
            text("{89ABCDEF-0123-4567-89AB-CDEF01234567}"),
            text("line1\nline2"),
            text(" "),
        ],
        vec![
            Value::Null,
            text("cr\rhere"),
            Value::Null,
            Value::Null,
            Value::Double(1.0e7),
            Value::Float(1.0e7),
            Value::Null,
            Value::Bool(true),
            date(2022, 6, 7, 8, 9, 10),
            bytes(&(0u8..30).collect::<Vec<u8>>()),
            Value::Null,
            Value::Null,
            text("x"),
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
            date(1999, 12, 31, 23, 59, 59),
            bytes(&[]),
            Value::Null,
            text(" tab\tand semicolon; "),
            Value::Null,
        ],
        vec![
            Value::Long(42),
            text(""),
            Value::Long(1),
            money(1234567891),
            Value::Double(-0.0),
            Value::Float(-0.0),
            Value::Long(127),
            Value::Bool(false),
            date(2000, 2, 29, 0, 0, 0),
            bytes(&[0xFF; 30]),
            Value::Null,
            text("multi\r\nline"),
            text("end"),
        ],
        vec![
            Value::Long(7),
            text("semi;colon"),
            Value::Long(2),
            money(-1),
            Value::Double(1234567.0),
            Value::Float(1234.5),
            Value::Long(-128),
            Value::Bool(true),
            date(2001, 9, 9, 1, 2, 3),
            bytes(&[1, 2, 3, 4, 5]),
            Value::Null,
            text(""),
            Value::Null,
        ],
        vec![
            Value::Long(8),
            text("it's"),
            Value::Long(3),
            money(15000),
            Value::Double(-1.25),
            Value::Float(1234567.0),
            Value::Null,
            Value::Bool(true),
            date(2023, 12, 31, 23, 59, 59),
            bytes(&[9]),
            Value::Null,
            text("tail"),
            Value::Null,
        ],
    ];
    for row in customers {
        db.add_rows("Customers", &[row])?;
    }

    db.create_table(&TableSpec::new(
        "Orders",
        vec![
            ColumnSpec::new("Id", DataType::Long),
            ColumnSpec::new("Customer", DataType::Long),
            ColumnSpec::new("Note", DataType::Memo),
            ColumnSpec::new("Placed", DataType::ShortDateTime),
            ColumnSpec::new("Total", DataType::Money),
            ColumnSpec::new("Raw", DataType::Binary).with_length(10),
            ColumnSpec::new("Flag", DataType::Boolean),
        ],
    ))?;
    let orders = vec![
        vec![
            Value::Long(10),
            Value::Long(1),
            text("first, order"),
            date(2020, 1, 1, 0, 0, 0),
            money(123400),
            bytes(&[1, 2]),
            Value::Bool(true),
        ],
        vec![
            Value::Long(11),
            Value::Long(2),
            text("line1\nline2"),
            date(2021, 2, 3, 4, 5, 6),
            money(-55000),
            Value::Null,
            Value::Bool(false),
        ],
        vec![
            Value::Long(12),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            bytes(&[]),
            Value::Null,
        ],
        vec![
            Value::Null,
            Value::Long(3),
            text("quote \" q"),
            date(2022, 6, 7, 8, 9, 10),
            money(0),
            bytes(&[0, 255]),
            Value::Bool(true),
        ],
    ];
    for row in orders {
        db.add_rows("Orders", &[row])?;
    }

    db.create_table(&TableSpec::new(
        "Empty",
        vec![
            ColumnSpec::new("A", DataType::Long),
            ColumnSpec::new("B", DataType::Text).with_length(20),
        ],
    ))?;

    db.create_table(&TableSpec::new(
        "R&D #2 (100%)",
        vec![
            ColumnSpec::new("A", DataType::Long),
            ColumnSpec::new("Label", DataType::Text).with_length(50),
            ColumnSpec::new("Note", DataType::Memo),
        ],
    ))?;
    let weird = vec![
        vec![Value::Long(1), text("comma, value"), text("memo\nline")],
        vec![Value::Long(2), text("quote \" value"), Value::Null],
        vec![Value::Null, text(""), text(" ")],
    ];
    for row in weird {
        db.add_rows("R&D #2 (100%)", &[row])?;
    }

    db.create_table(&TableSpec::new(
        "Ünïcode 表",
        vec![
            ColumnSpec::new("Id", DataType::Long),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
        ],
    ))?;
    let unicode = vec![
        vec![Value::Long(1), text("café")],
        vec![Value::Long(2), text("naïve 表")],
        vec![Value::Long(3), text("plain")],
    ];
    for row in unicode {
        db.add_rows("Ünïcode 表", &[row])?;
    }

    db.flush()?;
    println!("wrote {}", path.display());
    Ok(())
}

fn run_empty(path: &Path, fmt: &str) -> Result<()> {
    let mut db = create(path, fmt)?;
    db.flush()?;
    println!("wrote empty {}", path.display());
    Ok(())
}

fn run_hostile(path: &Path, fmt: &str) -> Result<()> {
    let mut db = create(path, fmt)?;
    for name in ["Alpha", "Questions?Mark", "Zulu"] {
        db.create_table(&TableSpec::new(
            name,
            vec![
                ColumnSpec::new("A", DataType::Long),
                ColumnSpec::new("Note", DataType::Text).with_length(100),
            ],
        ))?;
        db.add_rows(
            name,
            &[vec![Value::Long(1), text(&format!("row of {name}"))]],
        )?;
    }
    db.flush()?;
    println!("wrote hostile {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// exportAll
// ---------------------------------------------------------------------------

/// `AllTablesFilter`: drops a column named "Note" where present and skips rows
/// whose first value is null (both mirror the Java filter).
struct AllTablesFilter;

impl ExportFilter for AllTablesFilter {
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

fn mode_options(mode: &str) -> Result<(&'static str, ExportOptions)> {
    let opts = match mode {
        "default" | "overwrite" => return Ok(("csv", ExportOptions::default())),
        "ext" => return Ok(("dat", ExportOptions::default())),
        "header" => ("csv", ExportOptions::new().with_header(true)),
        "txt" => (
            "txt",
            ExportOptions::new()
                .with_header(true)
                .with_delimiter(";")
                .with_quote('\'')
                .with_filter(Box::new(AllTablesFilter)),
        ),
        "tsv" => (
            "tsv",
            ExportOptions::new()
                .with_header(true)
                .with_delimiter("\t")
                .with_quote('"'),
        ),
        "filter" => (
            "csv",
            ExportOptions::new()
                .with_header(true)
                .with_filter(Box::new(AllTablesFilter)),
        ),
        "builder" => (
            "bld",
            ExportOptions::new()
                .with_header(true)
                .with_delimiter("|")
                .with_quote('\'')
                .with_filter(Box::new(AllTablesFilter)),
        ),
        other => {
            return Err(Error::IllegalArgument(format!(
                "unknown exportAll mode {other}"
            )))
        }
    };
    Ok(opts)
}

fn stale_bytes() -> Vec<u8> {
    vec![b'X'; 5000]
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn manifest_text(mode: &str, ext: &str, order: &[String], dir: &Path, ok: bool) -> Result<String> {
    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Ok(name) = entry.file_name().into_string() {
                files.insert(name, path);
            }
        }
    }

    let mut out = String::new();
    out.push_str(&format!("mode\t{mode}\n"));
    out.push_str(&format!("ext\t{ext}\n"));
    for (i, table) in order.iter().enumerate() {
        out.push_str(&format!("order\t{i}\t{table}\n"));
    }
    out.push_str(&format!("status\t{}\n", if ok { "ok" } else { "error" }));
    for table in order {
        let name = export_file_name(table, ext);
        match files.get(&name) {
            Some(path) => {
                let bytes = std::fs::read(path)?;
                out.push_str(&format!(
                    "file\t{name}\ttrue\t{}\t{:016x}\n",
                    bytes.len(),
                    fnv1a(&bytes)
                ));
            }
            None => out.push_str(&format!("file\t{name}\tfalse\t-1\t-\n")),
        }
    }
    Ok(out)
}

fn run_export_all(
    db_path: &Path,
    dir: &Path,
    mode: &str,
    manifest: &Path,
    use_map: bool,
) -> Result<()> {
    let (ext, opts) = mode_options(mode)?;
    let mut db = Database::open_path(db_path)?;

    if mode == "overwrite" {
        std::fs::write(dir.join("Customers.csv"), stale_bytes())?;
        std::fs::write(dir.join("Orders.csv"), stale_bytes())?;
    }

    let result = if use_map {
        export_all_to_map(&mut db, &opts).and_then(|map| {
            for (table, text) in &map {
                std::fs::write(dir.join(export_file_name(table, ext)), text)?;
            }
            Ok(())
        })
    } else {
        match mode {
            "default" | "overwrite" => export_all(&mut db, dir),
            "ext" => export_all_ext(&mut db, dir, ext),
            _ => export_all_with(&mut db, dir, ext, &opts),
        }
    };

    let order = db.get_table_names()?;
    let text = manifest_text(mode, ext, &order, dir, result.is_ok())?;
    std::fs::write(manifest, text)?;
    if let Err(e) = result {
        println!(
            "status=error tables={} manifest={}",
            order.len(),
            manifest.display()
        );
        eprintln!("export error: {e}");
    } else {
        println!(
            "status=ok tables={} manifest={}",
            order.len(),
            manifest.display()
        );
    }
    Ok(())
}

fn run_append(db_path: &Path, file: &Path, table: &str) -> Result<()> {
    let mut db = Database::open_path(db_path)?;
    if file.exists() {
        std::fs::remove_file(file)?;
    }
    std::fs::write(file, b"PREFIX-LINE\n")?;
    let handle = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    let mut out = std::io::BufWriter::new(handle);
    export_table_to_writer(
        &mut db,
        table,
        &mut out,
        &ExportOptions::new().with_header(true),
    )?;
    out.flush()?;
    drop(out);
    println!("appended {}", file.display());
    Ok(())
}
