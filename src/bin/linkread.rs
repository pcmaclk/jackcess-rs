//! `linkread` - Rust side of the linked-table reading scenario.
//!
//! Creates the same two databases as `java-src/LinkReadGen.java`:
//!
//! * `link_target.<ext>`: `Source(ID long pk, Name text, Num long)` + 3 rows;
//! * `link_linker.<ext>`: two linked tables (`LinkSource`, `LinkSourceCopy`)
//!   pointing at the target by *relative* file name, plus the local
//!   `Copy(ID long, Name text, Num long)` table.
//!
//! It then installs the unrestricted link resolver, checks `is_linked_table` /
//! `get_linked_databases`, reads every row through both links and inserts them
//! (link order, then table row order) into `Copy`.  The linker file therefore
//! depends on the linked read path, exactly like the Java reference.
//!
//! The catalog creation timestamps are read back from the Java file by the
//! harness (`LinkReadGen` prints them) and passed in with `--now-ms`, in
//! creation order: `Source,LinkSource,LinkSourceCopy,Copy`.
//!
//! Relative linkee names resolve against the process working directory, like
//! Java's `new File(linkeeName)`, so run this with the output directory as the
//! working directory.
//!
//! Usage: `linkread <outDir> <V2003|V2007|V2016> [--now-ms t1,t2,t3,t4]`

use std::path::PathBuf;

use jackcess::column::ColumnSpec;
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::format::FileFormat;
use jackcess::linked;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: linkread <outDir> <V2003|V2007|V2016> [--now-ms t1,t2,t3,t4]");
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
    if now_ms.len() != 4 {
        eprintln!(
            "usage: linkread ... --now-ms <Source,LinkSource,LinkSourceCopy,Copy> \
             (got {} timestamps)",
            now_ms.len()
        );
        std::process::exit(2);
    }

    if let Err(e) = run(&out_dir, format, &now_ms) {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn source_spec() -> TableSpec {
    TableSpec::new(
        "Source",
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

fn run(out_dir: &std::path::Path, format: FileFormat, dates: &[i64]) -> jackcess::Result<()> {
    let ext = format.extension();
    let linkee_name = format!("link_target{}", ext);
    let target_path = out_dir.join(&linkee_name);
    let linker_path = out_dir.join(format!("link_linker{}", ext));
    for path in [&target_path, &linker_path] {
        if path.exists() {
            std::fs::remove_file(path)?;
        }
    }

    // ---- target database: Source(ID pk, Name, Num) + 3 rows -------------
    {
        let mut target = Database::create(format, &target_path)?;
        // the fixed creation timestamp of `Source` (Java printed it)
        target.now_ms = Some(dates[0]);
        target.now_queue = vec![dates[0]];
        target.create_table(&source_spec())?;
        // one row at a time, like Java's `Table.addRow` loop (the batch
        // `Database::add_rows` writes different index page histories)
        target.add_rows(
            "Source",
            &[vec![
                Value::Long(1),
                Value::Text("alpha".to_string()),
                Value::Long(10),
            ]],
        )?;
        target.add_rows(
            "Source",
            &[vec![
                Value::Long(2),
                Value::Text("beta".to_string()),
                Value::Long(20),
            ]],
        )?;
        target.add_rows(
            "Source",
            &[vec![
                Value::Long(3),
                Value::Text("gamma".to_string()),
                Value::Long(30),
            ]],
        )?;
        target.flush()?;
    }

    // ---- linker database: two links + local Copy table ------------------
    let mut linker = Database::create(format, &linker_path)?;
    // creation order: LinkSource, LinkSourceCopy, Copy
    linker.now_ms = Some(dates[1]);
    linker.now_queue = dates[1..4].to_vec();
    linker.create_linked_table("LinkSource", &linkee_name, "Source")?;
    linker.create_linked_table("LinkSourceCopy", &linkee_name, "Source")?;
    linker.create_table(&copy_spec())?;

    // `LinkResolver.UNRESTRICTED`
    linker.set_link_resolver(linked::UNRESTRICTED);
    if !linker.is_linked_table("LinkSource") {
        return Err(jackcess::Error::IllegalState(
            "is_linked_table(LinkSource) is false".to_string(),
        ));
    }
    if linker.is_linked_table("Copy") {
        return Err(jackcess::Error::IllegalState(
            "is_linked_table(Copy) is true".to_string(),
        ));
    }

    // verify the linked metadata before reading anything; `get_table` returns
    // the table from the linkee database
    {
        let table = linker.get_table("LinkSource")?;
        if table.name != "Source" || table.columns.len() != 3 {
            return Err(jackcess::Error::IllegalState(format!(
                "unexpected linked table {} with {} columns",
                table.name,
                table.columns.len()
            )));
        }
    }
    let linked_names: Vec<String> = linker.get_linked_databases().keys().cloned().collect();
    if linked_names != vec![linkee_name.clone()] {
        return Err(jackcess::Error::IllegalState(format!(
            "get_linked_databases() = {:?}, expected [{}]",
            linked_names, linkee_name
        )));
    }

    // read all rows through each link, in deterministic table row order, and
    // insert them into Copy
    let mut copied = 0usize;
    for link_name in ["LinkSource", "LinkSourceCopy"] {
        let rows = linker.read_rows_with_ids(link_name)?;
        for (_, values) in rows {
            let values = values.ok_or_else(|| {
                jackcess::Error::IllegalState(format!(
                    "a row read through {} is deleted",
                    link_name
                ))
            })?;
            linker.add_rows("Copy", &[values])?;
            copied += 1;
        }
    }
    if copied != 6 {
        return Err(jackcess::Error::IllegalState(format!(
            "copied {} rows, expected 6",
            copied
        )));
    }
    linker.flush()?;

    println!("linked databases: {:?}", linked_names);
    println!("copied {} rows", copied);
    Ok(())
}
