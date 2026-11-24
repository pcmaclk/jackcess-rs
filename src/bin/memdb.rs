//! `memdb` - generates the in-memory database reference scenario.
//!
//! This is the Rust counterpart of `java-src/MemDbGen.java`: it builds a
//! database entirely over [`jackcess::mem::MemFileChannel`] (tables, indexes,
//! rows, MEMO values and deleted rows), writes the resulting bytes to a file,
//! and then checks that the bytes can be reopened in memory and read back with
//! exactly the same rows as the file which was just written.
//!
//! The catalog timestamps (`DateCreate`/`DateUpdate` of the two tables) come
//! from the Java reference database, like every other harness:
//! `--now-ms` takes the millisecond values read back by `GetDates`.
//!
//! Usage:
//!   memdb <out.mdb|out.accdb> [V2003|V2007|V2010|V2016] [--now-ms ms[,ms...]]

use jackcess::column::ColumnSpec;
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::format::FileFormat;
use jackcess::mem::{self, MemDatabase};
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: memdb <out.mdb> [format] [--now-ms N[,N...]]");
        std::process::exit(2);
    }
    if let Err(err) = run(&args) {
        eprintln!("memdb: {err}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> jackcess::Result<()> {
    let out = args[1].clone();
    let mut fmt = FileFormat::V2003;
    let mut now_ms: Option<i64> = None;
    let mut now_queue: Vec<i64> = Vec::new();

    let mut i = 2;
    while i < args.len() {
        if args[i] == "--now-ms" {
            let raw = args.get(i + 1).cloned().unwrap_or_default();
            let values: Vec<i64> = raw
                .split(',')
                .filter(|part| !part.trim().is_empty())
                .map(|part| {
                    part.trim().parse::<i64>().map_err(|_| {
                        jackcess::Error::IllegalArgument(format!(
                            "invalid --now-ms value '{}'",
                            part
                        ))
                    })
                })
                .collect::<jackcess::Result<Vec<_>>>()?;
            if values.is_empty() {
                return Err(jackcess::Error::IllegalArgument(format!(
                    "invalid --now-ms value '{}'",
                    raw
                )));
            }
            now_ms = Some(values[0]);
            if values.len() > 1 {
                now_queue = values;
            }
            i += 2;
        } else if let Some(format) = FileFormat::from_name(&args[i]) {
            fmt = format;
            i += 1;
        } else {
            return Err(jackcess::Error::IllegalArgument(format!(
                "unknown argument '{}'",
                args[i]
            )));
        }
    }

    // the whole database lives in memory; no output file exists until the
    // bytes are handed over at the end
    let mut db = mem::create(fmt)?;
    db.now_ms = now_ms;
    db.now_queue = now_queue;
    build(&mut db)?;
    db.flush()?;
    let bytes = db.to_bytes();
    std::fs::write(&out, &bytes)?;

    // (c) reopen the bytes in the memory channel and compare every row with the
    // file which was just written
    let mut from_memory = mem::open(&bytes)?;
    let mut from_disk = Database::open_path(std::path::Path::new(&out))?;
    let mut mismatches = 0usize;
    let mut counts = Vec::new();
    for table in ["Test", "BigMemo"] {
        let memory_rows = from_memory.read_rows_with_ids(table)?;
        let disk_rows = from_disk.read_rows_with_ids(table)?;
        // `read_rows_with_ids` keeps the deleted row slots (with a `None`
        // value), so report the live rows
        counts.push(format!(
            "{}={}",
            table,
            memory_rows
                .iter()
                .filter(|(_, values)| values.is_some())
                .count()
        ));
        if memory_rows != disk_rows {
            eprintln!(
                "memdb: {} rows read from memory do not match the file",
                table
            );
            mismatches += 1;
        }
        let memory_indexes = index_names(&mut from_memory, table)?;
        let disk_indexes = index_names(&mut from_disk, table)?;
        if memory_indexes != disk_indexes {
            eprintln!(
                "memdb: {} indexes differ: memory {:?}, file {:?}",
                table, memory_indexes, disk_indexes
            );
            mismatches += 1;
        }
    }

    // the deletion scenario must really have removed its rows
    let remaining: Vec<i64> = from_memory
        .read_rows_with_ids("Test")?
        .into_iter()
        .filter_map(|(_, values)| values)
        .filter_map(|values| values.first().and_then(|v| v.as_i64()))
        .collect();
    for deleted in [3i64, 8, 40, 71, 99, 150] {
        if remaining.contains(&deleted) {
            eprintln!("memdb: deleted row id {} is still present", deleted);
            mismatches += 1;
        }
    }

    println!("wrote {} size={}", out, bytes.len());
    println!("tables: {}", counts.join(" "));
    println!("mem-open-matches-disk: {}", mismatches == 0);
    if mismatches != 0 {
        return Err(jackcess::Error::Jackcess(format!(
            "{} read back mismatch(es)",
            mismatches
        )));
    }
    Ok(())
}

fn index_names(db: &mut Database, table: &str) -> jackcess::Result<Vec<String>> {
    let t = db.get_table(table)?;
    Ok(t.indexes.iter().map(|index| index.name.clone()).collect())
}

// ---------------------------------------------------------------------------
// the reference scenario: the `delete` and `bigmemo` scenarios of
// `java-src/Gen.java`, composed in one database.  Both are mirrored one for one
// so that the page history (and therefore every written page) is the same.
// ---------------------------------------------------------------------------

fn build(db: &mut MemDatabase) -> jackcess::Result<()> {
    // `Gen.makeDeleteTable` + `Gen.addDeleteRows` + `Gen.runDeleteScenario`
    let spec = TableSpec::new(
        "Test",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Note", DataType::Memo),
            ColumnSpec::new("Value", DataType::Double),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    let mut rows = Vec::new();
    for i in 0..150i64 {
        rows.push(vec![
            Value::Null,
            Value::Text(format!("name{}", i)),
            Value::Text(if i % 7 == 0 {
                long_memo(i)
            } else {
                format!("memo {}", i)
            }),
            Value::Double(i as f64 * 1.5 + 0.25),
        ]);
    }
    add_rows_one_by_one(db, "Test", &rows)?;

    let id_col = db.get_table("Test")?.column_index("ID").unwrap_or(0);
    let mut by_id = std::collections::HashMap::new();
    for (row_id, values) in db.read_rows_with_ids("Test")? {
        if let Some(values) = values {
            if let Some(Value::Long(id)) = values.get(id_col) {
                by_id.insert(*id, row_id);
            }
        }
    }
    for id in [3i32, 8, 40, 71, 99, 150] {
        let row_id = *by_id.get(&id).ok_or_else(|| {
            jackcess::Error::Jackcess(format!("memdb scenario: row {} not found", id))
        })?;
        db.delete_row("Test", row_id)?;
    }

    let after = vec![
        vec![
            Value::Null,
            Value::Text("after0".to_string()),
            Value::Text("after memo 0".to_string()),
            Value::Double(1000.5),
        ],
        vec![
            Value::Null,
            Value::Text("after1".to_string()),
            Value::Text(format!("LM:{}", "0123456789".repeat(120))),
            Value::Double(1001.5),
        ],
        vec![
            Value::Null,
            Value::Text("after2".to_string()),
            Value::Text("after memo 2".to_string()),
            Value::Double(1002.5),
        ],
    ];
    add_rows_one_by_one(db, "Test", &after)?;

    // `Gen.makeBigMemo` + `Gen.addBigMemoRows`: MEMO values which span long
    // value pages
    let spec = TableSpec::new(
        "BigMemo",
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Body", DataType::Memo),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;

    let mut rows = Vec::new();
    for size in [10usize, 1000, 3000, 6000, 12000] {
        let mut body = String::with_capacity(size);
        for j in 0..size {
            body.push((b'a' + (j % 26) as u8) as char);
        }
        rows.push(vec![Value::Null, Value::Text(body)]);
    }
    add_rows_one_by_one(db, "BigMemo", &rows)?;
    Ok(())
}

/// The long MEMO value of the delete scenario (`Gen.longMemo`).
fn long_memo(tag: i64) -> String {
    format!("L{}:{}", tag, "abcdefghij".repeat(300))
}

fn add_rows_one_by_one(
    db: &mut MemDatabase,
    table: &str,
    rows: &[Vec<Value>],
) -> jackcess::Result<()> {
    // one `addRowFromMap` per row, like the Java generator, so every row goes
    // through the same page/index write path
    for row in rows {
        db.add_rows(table, std::slice::from_ref(row))?;
    }
    Ok(())
}
