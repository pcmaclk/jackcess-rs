//! `attachgen` - the Rust side of the attachment scenario.
//!
//! Mirrors `java-src/AttachGen.java` byte for byte: an ACE database with a
//! table which holds an attachment column, and attachments whose file types
//! access stores raw (png/jpg/zip/docx/xlsx/...) as well as file types it
//! deflates (txt/csv/pdf/html/bin/...), upper case extensions, attachments
//! without a timestamp/flags or with a URL, empty payloads and large payloads.
//!
//! The flat table of an attachment column has a random GUID name, so the Java
//! file's name is read back (`ComplexInfo --flat-names`) and passed in with
//! `--complex-names`; the catalog timestamps are read back with `GetDates` and
//! passed in with `--now-ms` (see `tools/verify-attach.ps1`).
//!
//! Usage: `attachgen <out.accdb> [V2007|V2016] [--now-ms a,b,..]
//! [--complex-names names]`

use jackcess::column::ColumnSpec;
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::datenums::LocalDateTime;
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!(
            "usage: attachgen <out.accdb> [V2007|V2016] [--now-ms a,b,..] \
             [--complex-names names]"
        );
        std::process::exit(2);
    }
    let out = args[1].clone();
    let mut file_format = FileFormat::V2007;
    let mut now_ms: Option<i64> = None;
    let mut now_queue: Vec<i64> = Vec::new();
    let mut complex_names: Vec<String> = Vec::new();
    let mut i = 2;
    while i < args.len() {
        if args[i] == "--now-ms" {
            let raw = args.get(i + 1).cloned().unwrap_or_default();
            now_queue = raw
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            now_ms = now_queue.first().copied();
            i += 2;
        } else if args[i] == "--complex-names" {
            let raw = args.get(i + 1).cloned().unwrap_or_default();
            complex_names = raw
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
            i += 2;
        } else {
            if let Some(f) = FileFormat::from_name(&args[i]) {
                file_format = f;
            }
            i += 1;
        }
    }

    if let Err(e) = run(
        std::path::Path::new(&out),
        file_format,
        now_ms,
        &now_queue,
        &complex_names,
    ) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn run(
    path: &std::path::Path,
    fmt: FileFormat,
    now_ms: Option<i64>,
    now_queue: &[i64],
    complex_names: &[String],
) -> jackcess::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    let mut db = Database::create(fmt, path)?;
    db.now_ms = now_ms;
    db.now_queue = now_queue.to_vec();
    db.set_complex_names(complex_names.to_vec());

    // the table, exactly like the Java `TableBuilder`
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
    db.create_table(&spec)?;

    for name in ["zero", "raw", "compressed", "mixed", "large", "empty"] {
        add_rows_one_by_one(
            &mut db,
            "Test",
            &[vec![Value::Null, Value::Text(name.to_string())]],
        )?;
    }

    // walk the rows in table order, like `for (Row row : t)` in the Java
    // generator, and add the attachments of each row
    let rows = db.read_rows_with_ids("Test")?;
    for (row_id, values) in rows {
        let name = values
            .as_ref()
            .and_then(|v| v.get(1))
            .and_then(|v| v.as_text())
            .unwrap_or("")
            .to_string();
        let mut files = db.complex_value_fk("Test", row_id, "Files")?;
        match name.as_str() {
            "raw" => {
                // the file types access stores raw
                files.add_attachment(
                    None,
                    Some("small.png"),
                    Some("png"),
                    &seq_bytes(40, 1),
                    Some(date(2021, 5, 4, 12, 34, 56)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("two.jpg"),
                    Some("jpg"),
                    &seq_bytes(300, 3),
                    Some(date(2021, 7, 8, 9, 10, 11)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("third.zip"),
                    Some("zip"),
                    &seq_bytes(10, 4),
                    Some(date(2021, 8, 9, 10, 11, 12)),
                    Some(0),
                )?;
            }
            "compressed" => {
                // the file types access deflates
                files.add_attachment(
                    None,
                    Some("note.txt"),
                    Some("txt"),
                    &note_text(50),
                    Some(date(2021, 9, 10, 11, 12, 13)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("data.csv"),
                    Some("csv"),
                    &csv_text(100),
                    Some(date(2021, 10, 11, 12, 13, 14)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("report.pdf"),
                    Some("pdf"),
                    &lcg_bytes(5000, 99),
                    Some(date(2021, 11, 12, 13, 14, 15)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("page.html"),
                    Some("html"),
                    &html_text(40),
                    Some(date(2021, 12, 13, 14, 15, 16)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("empty.txt"),
                    Some("txt"),
                    &[],
                    Some(date(2022, 1, 14, 15, 16, 17)),
                    Some(0),
                )?;
            }
            "mixed" => {
                // raw formats, mixed case extensions, an attachment without
                // metadata and one with a URL
                files.add_attachment(
                    None,
                    Some("letter.docx"),
                    Some("docx"),
                    &lcg_bytes(1000, 7),
                    Some(date(2022, 2, 15, 16, 17, 18)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("sheet.xlsx"),
                    Some("xlsx"),
                    &lcg_bytes(1200, 8),
                    Some(date(2022, 3, 16, 17, 18, 19)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("UPPER.PNG"),
                    Some("PNG"),
                    &lcg_bytes(500, 10),
                    Some(date(2022, 4, 17, 18, 19, 20)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("UPPER.TXT"),
                    Some("TXT"),
                    &note_text(10),
                    Some(date(2022, 5, 18, 19, 20, 21)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("blob.bin"),
                    Some("bin"),
                    &lcg_bytes(2000, 9),
                    Some(date(2022, 6, 19, 20, 21, 22)),
                    Some(0),
                )?;
                // no timestamp and no flags
                files.add_attachment(
                    None,
                    Some("no-meta.txt"),
                    Some("txt"),
                    &note_text(3),
                    None,
                    None,
                )?;
                // an attachment which carries a URL
                files.add_attachment(
                    Some("http://example.com/url.txt"),
                    Some("url.txt"),
                    Some("txt"),
                    &note_text(5),
                    Some(date(2022, 7, 20, 21, 22, 23)),
                    Some(0),
                )?;
            }
            "large" => {
                // large payloads: text compresses well, binary does not
                files.add_attachment(
                    None,
                    Some("big.txt"),
                    Some("txt"),
                    &big_text(1000),
                    Some(date(2022, 8, 21, 22, 23, 24)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("big.bin"),
                    Some("bin"),
                    &lcg_bytes(20000, 11),
                    Some(date(2022, 9, 22, 23, 0, 1)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("big.png"),
                    Some("png"),
                    &lcg_bytes(30000, 12),
                    Some(date(2022, 10, 23, 0, 1, 2)),
                    Some(0),
                )?;
            }
            "empty" => {
                files.add_attachment(
                    None,
                    Some("zero.bin"),
                    Some("bin"),
                    &[],
                    Some(date(2022, 11, 24, 1, 2, 3)),
                    Some(0),
                )?;
                files.add_attachment(
                    None,
                    Some("zero.jpg"),
                    Some("jpg"),
                    &[],
                    Some(date(2022, 12, 25, 2, 3, 4)),
                    Some(0),
                )?;
            }
            _ => {}
        }
    }

    db.flush()?;
    println!(
        "wrote {} size={}",
        path.display(),
        std::fs::metadata(path)?.len()
    );
    Ok(())
}

/// Adds rows one at a time, which is what `Gen.java`/`AttachGen.java` do with
/// `Table.addRowFromMap` (the per-row index write history is observable).
fn add_rows_one_by_one(
    db: &mut Database,
    table: &str,
    rows: &[Vec<Value>],
) -> jackcess::Result<()> {
    for row in rows {
        db.add_rows(table, std::slice::from_ref(row))?;
    }
    Ok(())
}

fn date(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> LocalDateTime {
    LocalDateTime::new(year, month, day, hour, minute, second, 0)
}

/// The same deterministic byte sequence as the Java generator.
fn seq_bytes(len: usize, seed: u32) -> Vec<u8> {
    (0..len)
        .map(|i| ((i as u32 * 7 + seed) & 0xFF) as u8)
        .collect()
}

/// A deterministic pseudo random sequence (32 bit LCG, as in the Java port).
fn lcg_bytes(len: usize, seed: i32) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            ((x as u32) >> 16) as u8
        })
        .collect()
}

fn note_text(lines: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..lines {
        s.push_str(&format!(
            "The quick brown fox jumps over the lazy dog. line {} of the attachment corpus.\n",
            i
        ));
    }
    s.into_bytes()
}

fn csv_text(rows: usize) -> Vec<u8> {
    let mut s = String::from("id,name,amount,note\n");
    for i in 0..rows {
        s.push_str(&format!(
            "{},item{},{},note {}\n",
            i,
            i % 97,
            i * 3 + 1,
            i % 13
        ));
    }
    s.into_bytes()
}

fn html_text(sections: usize) -> Vec<u8> {
    let mut s = String::from("<html><body>\n");
    for i in 0..sections {
        s.push_str(&format!(
            "<p id=\"p{}\">section {} content with some repeated words</p>\n",
            i, i
        ));
    }
    s.push_str("</body></html>\n");
    s.into_bytes()
}

fn big_text(lines: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..lines {
        s.push_str(&format!(
            "big attachment line {}: the quick brown fox jumps over the lazy dog.\n",
            i
        ));
    }
    s.into_bytes()
}
