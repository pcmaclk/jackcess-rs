//! `linkwrite` - Rust side of the linked-table writing scenario.
//!
//! Creates the same two databases as `java-src/LinkWriteGen.java`:
//!
//! * `linkw_target.<ext>`: `T(ID long pk, Name text(50), Num long)` + 3 rows;
//! * `linkw_linker.<ext>`: the linked table `L` pointing at the target by
//!   *relative* file name (the linkee table is `T`), plus the local
//!   `Copy(ID long, Name text(50), Num long)` table.
//!
//! It then installs the unrestricted link resolver and, through the link,
//! inserts rows 4 and 5, updates row 2 to ID 12 (an indexed key change, same
//! Name length), deletes row 3, inserts rows 6 and 7, checks that reading
//! through the same cached linkee sees every change, closes and *reopens* the
//! linker, reads every row through the link and copies it into `Copy`.
//!
//! Every linked write uses the database-level write API on the link name
//! (`add_rows`/`update_row`/`delete_row`), which resolves to the linkee
//! database and the linkee's table definition, uses the linkee's channel, and
//! never writes the linker file.  Each row is written with its own
//! `add_rows` batch of one row, matching Java's `Table.addRow` loop.
//!
//! The catalog creation timestamps are read back from the Java file by the
//! harness (`LinkWriteGen` prints them) and passed in with `--now-ms`, in
//! creation order: `T,L,Copy`.
//!
//! Relative linkee names resolve against the process working directory, like
//! Java's `new File(linkeeName)`, so run this with the output directory as the
//! working directory.
//!
//! Usage: `linkwrite <outDir> <V2003|V2007|V2016> [--now-ms t1,t2,t3]`

use std::path::{Path, PathBuf};

use jackcess::column::ColumnSpec;
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::error::{Error, Result};
use jackcess::format::FileFormat;
use jackcess::linked;
use jackcess::table::RowId;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: linkwrite <outDir> <V2003|V2007|V2016> [--now-ms t1,t2,t3]");
        std::process::exit(2);
    }
    let out_dir = PathBuf::from(&args[1]);
    let format = match FileFormat::from_name(&args[2]) {
        Some(format) => format,
        None => {
            eprintln!("unknown file format '{}'", args[2]);
            std::process::exit(2);
        }
    };
    let mut now_ms: Vec<i64> = Vec::new();
    let mut i = 3;
    while i < args.len() {
        if args[i] == "--now-ms" {
            now_ms = args
                .get(i + 1)
                .map(|raw| {
                    raw.split(',')
                        .filter_map(|s| s.trim().parse::<i64>().ok())
                        .collect()
                })
                .unwrap_or_default();
            i += 2;
        } else {
            eprintln!("unknown argument '{}'", args[i]);
            std::process::exit(2);
        }
    }
    if now_ms.len() != 3 {
        eprintln!(
            "usage: linkwrite ... --now-ms <T,L,Copy> (got {} timestamps)",
            now_ms.len()
        );
        std::process::exit(2);
    }

    if let Err(e) = run(&out_dir, format, &now_ms) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn t_spec() -> TableSpec {
    TableSpec::new(
        "T",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Num", DataType::Long),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ))
}

fn copy_spec() -> TableSpec {
    TableSpec::new(
        "Copy",
        vec![
            ColumnSpec::new("ID", DataType::Long),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Num", DataType::Long),
        ],
    )
}

fn row(id: i32, name: &str, num: i32) -> Vec<Value> {
    vec![
        Value::Long(id),
        Value::Text(name.to_string()),
        Value::Long(num),
    ]
}

/// The `RowId` of the row whose `ID` column holds `id`.
fn find_row_id(rows: &[(RowId, Option<Vec<Value>>)], id: i32) -> Result<RowId> {
    for (row_id, values) in rows {
        if let Some(values) = values {
            if values.first().and_then(|v| v.as_i64()) == Some(id as i64) {
                return Ok(*row_id);
            }
        }
    }
    Err(Error::IllegalState(format!("no row with ID {}", id)))
}

/// The `ID` values of a table in physical row order.
fn ids(rows: &[(RowId, Option<Vec<Value>>)]) -> Vec<i64> {
    rows.iter()
        .filter_map(|(_, values)| values.as_ref())
        .filter_map(|values| values.first().and_then(|v| v.as_i64()))
        .collect()
}

fn run(out_dir: &Path, format: FileFormat, dates: &[i64]) -> Result<()> {
    let ext = format.extension();
    let linkee_name = format!("linkw_target{}", ext);
    let target_path = out_dir.join(&linkee_name);
    let linker_path = out_dir.join(format!("linkw_linker{}", ext));
    for path in [&target_path, &linker_path] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }

    // ---- target database: T(ID pk, Name, Num) + 3 rows -------------------
    {
        let mut target = Database::create(format, &target_path)?;
        // the fixed creation timestamp of `T` (Java printed it)
        target.now_ms = Some(dates[0]);
        target.now_queue = vec![dates[0]];
        target.create_table(&t_spec())?;
        // one row at a time, like Java's `Table.addRow` loop
        for (id, name, num) in [(1, "one", 10), (2, "two", 20), (3, "three", 30)] {
            target.add_rows("T", &[row(id, name, num)])?;
        }
        target.flush()?;
    }

    // ---- linker database: one link + local Copy table --------------------
    {
        let mut linker = Database::create(format, &linker_path)?;
        // creation order: L, Copy
        linker.now_ms = Some(dates[1]);
        linker.now_queue = dates[1..3].to_vec();
        linker.create_linked_table("L", &linkee_name, "T")?;
        linker.create_table(&copy_spec())?;
        linker.flush()?;
    }
    let linker_after_create = std::fs::read(&linker_path)?;
    let target_after_create = std::fs::read(&target_path)?;

    // ---- writes through the link (the linkee's table) --------------------
    {
        let mut writer = Database::open_path(&linker_path)?;
        writer.set_link_resolver(linked::UNRESTRICTED);

        if !writer.is_linked_table("L") {
            return Err(Error::IllegalState("is_linked_table(L) is false".into()));
        }
        if writer.is_linked_table("Copy") {
            return Err(Error::IllegalState("is_linked_table(Copy) is true".into()));
        }
        // Java's `Table linked = writer.getTable("L")` opens (and caches) the
        // linkee before `getLinkedDatabases()` is queried
        {
            let table = writer.get_table("L")?;
            if table.name != "T" || table.columns.len() != 3 {
                return Err(Error::IllegalState(format!(
                    "unexpected linked table {} with {} columns",
                    table.name,
                    table.columns.len()
                )));
            }
        }
        let linked_names: Vec<String> = writer.get_linked_databases().keys().cloned().collect();
        if linked_names != vec![linkee_name.clone()] {
            return Err(Error::IllegalState(format!(
                "get_linked_databases() = {:?}, expected [{}]",
                linked_names, linkee_name
            )));
        }

        // insert through the link
        writer.add_rows("L", &[row(4, "four", 40)])?;
        writer.add_rows("L", &[row(5, "five", 50)])?;

        // update through the link, including an indexed key change (2 -> 12)
        let rows = writer.read_rows_with_ids("L")?;
        let id2 = find_row_id(&rows, 2)?;
        writer.update_row("L", id2, &row(12, "TWO", 22))?;

        // delete through the link
        let rows = writer.read_rows_with_ids("L")?;
        let id3 = find_row_id(&rows, 3)?;
        writer.delete_row("L", id3)?;

        // insert more through the link
        writer.add_rows("L", &[row(6, "six", 60)])?;
        writer.add_rows("L", &[row(7, "seven", 70)])?;

        // reading through the same (cached) linkee sees every change
        let rows = writer.read_rows_with_ids("L")?;
        if ids(&rows) != vec![1, 12, 4, 5, 6, 7] {
            return Err(Error::IllegalState(format!(
                "linked ids {:?} != [1, 12, 4, 5, 6, 7]",
                ids(&rows)
            )));
        }
        let name12 = rows
            .iter()
            .find(|(_, values)| {
                values
                    .as_ref()
                    .and_then(|v| v.first())
                    .and_then(|v| v.as_i64())
                    == Some(12)
            })
            .and_then(|(_, values)| values.as_ref())
            .and_then(|values| values.get(1))
            .and_then(|v| v.as_text())
            .map(|s| s.to_string());
        if name12.as_deref() != Some("TWO") {
            return Err(Error::IllegalState(format!("row 12 Name is {:?}", name12)));
        }
        // the linkee is still the only cached linked database
        if writer.get_linked_databases().len() != 1 {
            return Err(Error::IllegalState(format!(
                "get_linked_databases() has {} entries after the writes",
                writer.get_linked_databases().len()
            )));
        }
        writer.flush()?;
    }

    // writes through the link changed the linkee file, not the linker
    if std::fs::read(&linker_path)? != linker_after_create {
        return Err(Error::IllegalState(
            "writes through the link modified the linker file".into(),
        ));
    }
    if std::fs::read(&target_path)? == target_after_create {
        return Err(Error::IllegalState(
            "writes through the link did not modify the linkee file".into(),
        ));
    }

    // ---- reopen the linker, read everything back, copy into Copy ---------
    let mut copied = 0usize;
    {
        let mut reader = Database::open_path(&linker_path)?;
        reader.set_link_resolver(linked::UNRESTRICTED);
        let rows = reader.read_rows_with_ids("L")?;
        if ids(&rows) != vec![1, 12, 4, 5, 6, 7] {
            return Err(Error::IllegalState(format!(
                "rows read through the reopened link: {:?}",
                ids(&rows)
            )));
        }
        for (_, values) in rows {
            // `read_rows_with_ids` keeps the slot of a deleted row as `None`
            // (Java's `readAllRowsWithIds` reports deleted rows the same way);
            // the default iterator skips it
            let Some(values) = values else {
                continue;
            };
            reader.add_rows("Copy", &[values])?;
            copied += 1;
        }
        if copied != 6 {
            return Err(Error::IllegalState(format!(
                "copied {} rows, expected 6",
                copied
            )));
        }
        reader.flush()?;
    }

    // the target itself (not just the link) holds the written rows
    {
        let mut target = Database::open_path_read_only(&target_path)?;
        let rows = target.read_rows_with_ids("T")?;
        if ids(&rows) != vec![1, 12, 4, 5, 6, 7] {
            return Err(Error::IllegalState(format!(
                "rows read from the target: {:?}",
                ids(&rows)
            )));
        }
    }

    println!("linked databases: {:?}", vec![linkee_name]);
    println!("copied {} rows", copied);
    Ok(())
}
