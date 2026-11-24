//! `dbapi` - the Rust counterpart of `java-src/DbApiGen.java`: exercises the
//! [`jackcess::builder::DatabaseBuilder`] / `TableIterableBuilder` surface and
//! dumps the observed results in the canonical `key=value` form
//! `tools/verify-dbapi.ps1` compares against the Java reference.
//!
//! Modes:
//!
//! * `build <format> <out> [--now-ms N]` - creates a database through the
//!   builder (database/summary/user defined properties, a `T` and a `U` table
//!   built from the `new_table`/`new_column`/`new_index`/`new_primary_key`
//!   factories, a relationship from `new_relationship`, and rows).  Prints
//!   nothing; the verification compares the file bytes with the Java one.
//! * `props <db>` - dumps the database/summary/user defined property groups.
//! * `open <db> [--read-only] [--charset NAME] [--timezone ID] [--codec PW|-]`
//!   - opens the database through the builder and dumps the resulting options
//!     and a little data.
//! * `iter <db> [--open-linked]` - dumps the table/metadata iterables and the
//!   `TableMetaData.open`/`getTableDefinition` results.
//! * `meta <db> [--open-linked]` - only the metadata part of `iter`, for a
//!   database whose linked tables cannot be opened (the iterators themselves
//!   would fail on a real fixture whose linkee file is absent).
//! * `closebuild <format> <out>` - dumps the close behaviour on a freshly
//!   built database.
//! * `linker <format> <out>` - builds a target database and a linker database
//!   with a relative linked table (for the LINKED `TableMetaData.open` check).
//! * `encode <src> <dst> <keyHex>` - the `JetEncodingHandler.addEncoding`
//!   transform, so the harness can build an encoded database from the
//!   builder-created one and open it through `set_codec_provider`.
//! * `datefmt <pattern>` - formats/parses the fixed date list through
//!   `create_date_format`, mirroring the Java `SimpleDateFormat`.
//!
//! The output is sorted by the caller (`verify-dbapi.ps1` sorts both dumps),
//! so the lines are emitted in whatever order the work produces them.

use std::path::Path;

use jackcess::builder::{
    create_date_format, new_column, new_index, new_primary_key, new_relationship, new_table,
    DatabaseBuilder,
};
use jackcess::crypt::{CryptCodecProvider, JetEncodingHandler};
use jackcess::database::{Database, TableMetaData, TableMetaDataType};
use jackcess::datatype::DataType;
use jackcess::datenums::LocalDateTime;
use jackcess::error::{Error, Result};
use jackcess::format::{Charset, FileFormat};
use jackcess::property::{format_value, AUTHOR_PROP, TITLE_PROP};
use jackcess::tablecreator::{create_table, TableSpec};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let result = match args[1].as_str() {
        "build" => cmd_build(&args[2..]),
        "props" => cmd_props(&args[2..]),
        "open" => cmd_open(&args[2..]),
        "iter" => cmd_iter(&args[2..]),
        "meta" => cmd_meta(&args[2..]),
        "closebuild" => cmd_closebuild(&args[2..]),
        "linker" => cmd_linker(&args[2..]),
        "encode" => cmd_encode(&args[2..]),
        "datefmt" => cmd_datefmt(&args[2..]),
        other => {
            eprintln!("unknown mode {other}");
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
    eprintln!("usage: dbapi <build|props|open|iter|meta|closebuild|linker|encode|datefmt> ...");
}

fn out(key: &str, value: impl std::fmt::Display) {
    println!("{key}={value}");
}

fn bool_text(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn option_text(value: Option<&str>) -> &str {
    value.unwrap_or("null")
}

fn table_type_name(table_type: TableMetaDataType) -> &'static str {
    table_type.name()
}

fn charset_from_name(name: &str) -> Result<Charset> {
    match name.to_ascii_lowercase().as_str() {
        "utf-16le" | "utf16le" => Ok(Charset::Utf16Le),
        "utf-8" | "utf8" => Ok(Charset::Utf8),
        "windows-1252" | "cp1252" | "cp-1252" => Ok(Charset::Cp1252),
        other => Err(Error::IllegalArgument(format!(
            "Unsupported charset: {}",
            other
        ))),
    }
}

fn parse_format(name: &str) -> Result<FileFormat> {
    FileFormat::from_name(name)
        .ok_or_else(|| Error::IllegalArgument(format!("Unsupported file format: {}", name)))
}

// ---------------------------------------------------------------------------
// build
// ---------------------------------------------------------------------------

fn parse_now(args: &[String]) -> (Option<i64>, Vec<i64>) {
    let mut now_queue: Vec<i64> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--now-ms" {
            let raw = args.get(i + 1).cloned().unwrap_or_default();
            now_queue = raw
                .split(',')
                .filter_map(|s| s.trim().parse::<i64>().ok())
                .collect();
            i += 2;
        } else {
            i += 1;
        }
    }
    (now_queue.first().copied(), now_queue)
}

/// The primary side of the builder relationship (`T`).
fn t_spec() -> TableSpec {
    let mut spec = new_table("T").with_index(new_primary_key(&["ID"]));
    spec.columns = vec![
        new_column("ID", DataType::Long).with_auto_number(true),
        new_column("Name", DataType::Text).with_length(50),
        new_column("Value", DataType::Double),
    ];
    let mut by_name = new_index("ByName");
    by_name.columns = vec![("Name".to_string(), true)];
    spec.indexes.push(by_name.unique());
    spec
}

/// The secondary (foreign key) side of the builder relationship (`U`).
fn u_spec() -> TableSpec {
    let mut spec = new_table("U").with_index(new_primary_key(&["ID"]));
    spec.columns = vec![
        new_column("ID", DataType::Long).with_auto_number(true),
        new_column("TID", DataType::Long),
        new_column("Note", DataType::Text).with_length(30),
    ];
    spec
}

fn build_database(format: FileFormat, path: &Path, now_queue: &[i64]) -> Result<Database> {
    let builder = DatabaseBuilder::new_at(path)
        .set_file_format(format)
        .put_database_property_typed(
            TITLE_PROP,
            DataType::Text,
            Value::Text("Property DB".to_string()),
        )
        .put_database_property_typed("CustomNum", DataType::Long, Value::Long(12345))
        .put_summary_property_typed(
            TITLE_PROP,
            DataType::Text,
            Value::Text("Summary title".to_string()),
        )
        .put_summary_property_typed(
            AUTHOR_PROP,
            DataType::Text,
            Value::Text("the author".to_string()),
        )
        .put_user_defined_property_typed("MyBool", DataType::Boolean, Value::Bool(true))
        .put_user_defined_property_typed("MyNum", DataType::Long, Value::Long(7));
    let mut db = builder.create()?;
    db.now_ms = now_queue.first().copied();
    db.now_queue = now_queue.to_vec();

    create_table(&mut db, &t_spec())?;
    create_table(&mut db, &u_spec())?;
    db.add_relationship(
        new_relationship("T", "U")
            .add_columns("ID", "TID")
            .set_referential_integrity()
            .set_cascade_updates()
            .set_cascade_deletes(),
    )?;

    // the rows the Java reference adds, one at a time like `Gen.java`
    let t_rows: Vec<Vec<Value>> = vec![
        vec![Value::Null, Value::Text("alpha".into()), Value::Double(1.5)],
        vec![Value::Null, Value::Text("beta".into()), Value::Double(2.5)],
        vec![Value::Null, Value::Text("gamma".into()), Value::Double(3.5)],
    ];
    for row in &t_rows {
        db.add_rows("T", std::slice::from_ref(row))?;
    }
    let u_rows: Vec<Vec<Value>> = vec![
        vec![Value::Null, Value::Long(1), Value::Text("one".into())],
        vec![Value::Null, Value::Long(2), Value::Text("two".into())],
    ];
    for row in &u_rows {
        db.add_rows("U", std::slice::from_ref(row))?;
    }
    Ok(db)
}

fn cmd_build(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        return Err(Error::IllegalArgument(
            "usage: dbapi build <format> <out> [--now-ms N]".to_string(),
        ));
    }
    let format = parse_format(&args[0])?;
    let path = std::path::PathBuf::from(&args[1]);
    let (_ms, queue) = parse_now(&args[2..]);
    let mut db = build_database(format, &path, &queue)?;
    db.close()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// props
// ---------------------------------------------------------------------------

fn dump_property_group(name: &str, map: &jackcess::property::PropertyMap) {
    let mut props: Vec<(String, String)> = map
        .iter()
        .map(|prop| {
            (
                prop.name.clone(),
                format!(
                    "{}:{}",
                    prop.data_type.get_name(),
                    format_value(&prop.value)
                ),
            )
        })
        .collect();
    props.sort();
    for (prop, text) in props {
        out(&format!("prop.{}.{}", name, prop), text);
    }
}

fn cmd_props(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::IllegalArgument(
            "usage: dbapi props <db>".to_string(),
        ));
    }
    let mut db = DatabaseBuilder::open_path(&args[0])?;
    {
        let mut maps = db.get_database_properties()?;
        dump_property_group("db", maps.get_default());
    }
    {
        let mut maps = db.get_summary_properties()?;
        dump_property_group("summary", maps.get_default());
    }
    {
        let mut maps = db.get_user_defined_properties()?;
        dump_property_group("user", maps.get_default());
    }
    db.close()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// open
// ---------------------------------------------------------------------------

fn cmd_open(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::IllegalArgument(
            "usage: dbapi open <db> [--read-only] [--charset NAME] [--timezone ID] [--codec PW|-]"
                .to_string(),
        ));
    }
    let path = std::path::PathBuf::from(&args[0]);
    let mut read_only = false;
    let mut charset: Option<Charset> = None;
    let mut timezone: Option<String> = None;
    let mut codec: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--read-only" => {
                read_only = true;
                i += 1;
            }
            "--charset" => {
                charset = Some(charset_from_name(
                    args.get(i + 1).map(|s| s.as_str()).unwrap_or(""),
                )?);
                i += 2;
            }
            "--timezone" => {
                timezone = Some(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            "--codec" => {
                codec = Some(args.get(i + 1).cloned().unwrap_or_default());
                i += 2;
            }
            other => {
                return Err(Error::IllegalArgument(format!("unknown option {other}")));
            }
        }
    }

    let mut builder = DatabaseBuilder::new_at(&path).set_read_only(read_only);
    if let Some(charset) = charset {
        builder = builder.set_charset(charset);
    }
    if let Some(zone) = &timezone {
        builder = builder.set_time_zone_id(zone)?;
    }
    if let Some(password) = &codec {
        let password = if password == "-" {
            None
        } else {
            Some(password.as_str())
        };
        builder = builder.set_codec_provider(CryptCodecProvider::from_password(password));
    }
    let mut db = builder.open()?;

    out("db.file", db.get_name());
    out("db.charset", db.get_charset().name());
    out("db.zone", db.get_zone_id());
    out("db.zone_offset", db.get_zone_info().raw_offset_ms);
    // Java has no "is the zone named" flag; the harness compares the `UTC` /
    // `GMT±HH:MM` fixed offset ids against the `Area/City` form
    out("db.zone_named", bool_text(db.get_zone_id().contains('/')));
    out("db.read_only", bool_text(db.is_read_only()));
    out("db.file_format", format!("{:?}", db.get_file_format()?));
    out(
        "db.password",
        option_text(db.get_database_password()?.as_deref()),
    );
    out("db.eval_locale", db.get_eval_config().locale());
    out(
        "db.eval_zone",
        db.get_eval_config()
            .zone()
            .map(|zone| zone.zone_id())
            .unwrap_or_else(|| "null".to_string()),
    );
    out("db.users", db.get_table_names()?.join(","));
    out("db.system", db.get_system_table_names()?.join(","));

    // the rows of the `T` table the build mode creates (a fixture does not
    // have it)
    if db.find_table("T")?.is_some() {
        let rows = db.read_rows_with_ids("T")?;
        let mut values: Vec<String> = Vec::new();
        for (_, row) in &rows {
            if let Some(row) = row {
                let id = row.first().map(format_value).unwrap_or_default();
                let name = row.get(1).map(format_value).unwrap_or_default();
                values.push(format!("{}|{}", id, name));
            }
        }
        out("data.T.count", rows.len());
        out("data.T.rows", values.join(","));
    } else {
        out("data.T.count", "missing");
        out("data.T.rows", "missing");
    }

    db.close()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// iter
// ---------------------------------------------------------------------------

fn meta_text(meta: &TableMetaData) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}",
        meta.name,
        table_type_name(meta.table_type),
        bool_text(meta.is_system()),
        bool_text(meta.is_linked()),
        option_text(meta.linked_table_name.as_deref()),
        option_text(meta.linked_db_name.as_deref()),
        option_text(meta.connection_name.as_deref()),
    )
}

fn cmd_iter(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::IllegalArgument(
            "usage: dbapi iter <db> [--open-linked]".to_string(),
        ));
    }
    let open_linked = args.iter().any(|a| a == "--open-linked");
    let mut db = DatabaseBuilder::open_path(&args[0])?;
    if open_linked {
        db.set_link_resolver(jackcess::linked::UNRESTRICTED);
    }

    dump_meta(&mut db, open_linked)?;

    // `Database.iterator()` and the `newIterable()` filters.  Java's
    // `TableIterator` calls `getTable(name)`, so a linked catalog name yields
    // the *linkee* table (and its name); resolve the names the same way.
    let default_names = db.new_iterable().table_names(&mut db)?;
    out(
        "iter.default",
        resolve_names(&mut db, default_names.clone())?.join(","),
    );
    out("iter.default.count", default_names.len());
    let local_names = db
        .new_iterable()
        .with_local_user_tables_only()
        .table_names(&mut db)?;
    out("iter.local", resolve_names(&mut db, local_names)?.join(","));
    let system_names = db
        .new_iterable()
        .with_system_tables_only()
        .table_names(&mut db)?;
    out(
        "iter.system",
        resolve_names(&mut db, system_names)?.join(","),
    );
    let all_names = db
        .new_iterable()
        .set_include_system_tables(true)
        .table_names(&mut db)?;
    out("iter.all", resolve_names(&mut db, all_names)?.join(","));
    let no_link_names = db
        .new_iterable()
        .set_include_linked_tables(false)
        .table_names(&mut db)?;
    out(
        "iter.nolinked",
        resolve_names(&mut db, no_link_names)?.join(","),
    );

    // the callback iterator (`TableIterableBuilder.iterator()` equivalent),
    // limited to the local tables so a fixture whose linked database is absent
    // does not fail the check
    let mut visited: Vec<String> = Vec::new();
    db.new_iterable()
        .set_include_linked_tables(false)
        .for_each_table(&mut db, |table| {
            visited.push(table.name.clone());
            Ok(())
        })?;
    out("iter.visit", visited.join(","));

    // `Database.tables()`: the local tables in Java's iteration order
    let local_names: Vec<String> = db
        .tables()?
        .iter()
        .map(|table| table.name.clone())
        .collect();
    out("iter.tables", local_names.join(","));

    db.close()?;
    Ok(())
}

/// Resolves catalog names to the table names `Database.getTable(name)` returns:
/// a linked catalog name yields the linkee's table name, exactly like Java's
/// `TableIterator`.  A name which cannot be resolved keeps its catalog spelling
/// (Java would throw; the databases the harness runs `iter` on resolve every
/// name).
fn resolve_names(db: &mut Database, names: Vec<String>) -> Result<Vec<String>> {
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        let table_name = match db.find_table(&name) {
            Ok(Some(table)) => table.name.clone(),
            _ => name,
        };
        resolved.push(table_name);
    }
    Ok(resolved)
}

/// `meta <db> [--open-linked]`: only the metadata part of [`cmd_iter`], for
/// databases whose linked tables cannot be opened (a real fixture whose linkee
/// file is absent), where the iterators themselves would fail.
fn cmd_meta(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::IllegalArgument(
            "usage: dbapi meta <db> [--open-linked]".to_string(),
        ));
    }
    let open_linked = args.iter().any(|a| a == "--open-linked");
    let mut db = DatabaseBuilder::open_path(&args[0])?;
    if open_linked {
        db.set_link_resolver(jackcess::linked::UNRESTRICTED);
    }
    dump_meta(&mut db, open_linked)?;
    db.close()?;
    Ok(())
}

/// `Database.newTableMetaDataIterable()` plus
/// `TableMetaData.open`/`getTableDefinition` for every table.
fn dump_meta(db: &mut Database, open_linked: bool) -> Result<()> {
    let metas: Vec<TableMetaData> = db.new_table_meta_data_iterable()?.collect();
    for (index, meta) in metas.iter().enumerate() {
        out(&format!("meta.{}", index), meta_text(meta));
    }
    for meta in &metas {
        let name = meta.name.clone();
        let open = match meta.table_type {
            TableMetaDataType::Linked if !open_linked => "skipped".to_string(),
            _ => match meta.open(db) {
                Ok(Some(table)) => table.name.clone(),
                Ok(None) => "null".to_string(),
                Err(e) => format!("error:{}", error_class(&e)),
            },
        };
        out(&format!("open.{}", name), open);
        let def = match meta.get_table_definition(db) {
            Ok(Some(table)) => {
                let columns: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
                format!("{}[{}]", table.name, columns.join(","))
            }
            Ok(None) => "null".to_string(),
            Err(e) => format!("error:{}", error_class(&e)),
        };
        out(&format!("def.{}", name), def);
    }
    Ok(())
}

/// The Java exception simple name of a port error (the harness compares the
/// class names both sides report).
fn error_class(error: &Error) -> String {
    match error {
        Error::Io(io) => {
            let text = io.to_string();
            if text.contains("ClosedChannelException") {
                "ClosedChannelException".to_string()
            } else if text.contains("NonWritableChannelException") {
                "NonWritableChannelException".to_string()
            } else {
                "IOException".to_string()
            }
        }
        Error::IllegalArgument(_) => "IllegalArgumentException".to_string(),
        Error::IllegalState(_) => "IllegalStateException".to_string(),
        Error::Unsupported(_) => "UnsupportedOperationException".to_string(),
        Error::InvalidCredentials(_) => "InvalidCredentialsException".to_string(),
        Error::ConstraintViolation(_) => "ConstraintViolationException".to_string(),
        Error::InvalidValue(_) => "InvalidValueException".to_string(),
        Error::BatchUpdate { .. } => "BatchUpdateException".to_string(),
        Error::BufferUnderflow(_) => "BufferUnderflowException".to_string(),
        Error::BufferOverflow(_) => "BufferOverflowException".to_string(),
        Error::InvalidMark => "InvalidMarkException".to_string(),
        Error::NumberFormat(_) => "NumberFormatException".to_string(),
        Error::Jackcess(_) => "IOException".to_string(),
    }
}

// ---------------------------------------------------------------------------
// closebuild
// ---------------------------------------------------------------------------

fn cmd_closebuild(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        return Err(Error::IllegalArgument(
            "usage: dbapi closebuild <format> <out>".to_string(),
        ));
    }
    let format = parse_format(&args[0])?;
    let path = std::path::PathBuf::from(&args[1]);
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let mut db = DatabaseBuilder::new_at(&path)
        .set_file_format(format)
        .create()?;
    let mut spec = TableSpec::new("T", Vec::new());
    spec.columns = vec![new_column("ID", DataType::Long)];
    create_table(&mut db, &spec)?;
    db.add_rows("T", &[vec![Value::Long(1)]])?;

    out("is_open_before", bool_text(db.is_open()));
    db.close()?;
    out("close1", "ok");
    out("is_open_after", bool_text(db.is_open()));
    match db.close() {
        Ok(()) => out("close2", "ok"),
        Err(e) => out("close2", error_class(&e)),
    }
    match db.flush() {
        Ok(()) => out("flush_after_close", "ok"),
        Err(e) => out("flush_after_close", error_class(&e)),
    }

    let mut reopened = DatabaseBuilder::open_path(&path)?;
    out("rows_after_reopen", reopened.read_rows_with_ids("T")?.len());

    // reading after close: Java throws ClosedChannelException through the
    // closed channel, the port keeps serving the file (documented gap), so both
    // sides only report that the *build* path flushed the row
    out(
        "reopened_first_id",
        reopened
            .read_rows_with_ids("T")?
            .first()
            .and_then(|(_, row)| row.as_ref())
            .and_then(|row| row.first().cloned())
            .map(|value| format_value(&value))
            .unwrap_or_else(|| "null".to_string()),
    );
    reopened.close()?;

    let mut ro = DatabaseBuilder::new_at(&path).set_read_only(true).open()?;
    out("ro_read_only", bool_text(ro.is_read_only()));
    ro.close()?;
    out("ro_close", "ok");
    Ok(())
}

// ---------------------------------------------------------------------------
// linker
// ---------------------------------------------------------------------------

/// Creates a target database (`link_target.<ext>`) with one `Source` table and
/// a linker database (the given path) whose `LinkSource` table is a relative
/// link at the target, like `LinkReadGen` does.  The harness opens the linker
/// from its own directory with `LinkResolver.UNRESTRICTED` to exercise
/// `TableMetaData.open` on a LINKED table.
fn cmd_linker(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        return Err(Error::IllegalArgument(
            "usage: dbapi linker <format> <out>".to_string(),
        ));
    }
    let format = parse_format(&args[0])?;
    let out = std::path::PathBuf::from(&args[1]);
    let dir = out
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    std::fs::create_dir_all(&dir)?;
    let ext = format.extension();
    let target_path = dir.join(format!("link_target{}", ext));
    let _ = std::fs::remove_file(&target_path);
    let _ = std::fs::remove_file(&out);
    let target_name = format!("link_target{}", ext);

    {
        let mut target = DatabaseBuilder::create_file(format, &target_path)?;
        let mut source = new_table("Source").with_index(new_primary_key(&["ID"]));
        source.columns = vec![
            new_column("ID", DataType::Long),
            new_column("Name", DataType::Text).with_length(50),
        ];
        create_table(&mut target, &source)?;
        target.add_rows(
            "Source",
            &[
                vec![Value::Long(1), Value::Text("alpha".into())],
                vec![Value::Long(2), Value::Text("beta".into())],
            ],
        )?;
        target.close()?;
    }
    let mut linker = DatabaseBuilder::create_file(format, &out)?;
    linker.create_linked_table("LinkSource", &target_name, "Source")?;
    linker.close()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// encode
// ---------------------------------------------------------------------------

fn cmd_encode(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        return Err(Error::IllegalArgument(
            "usage: dbapi encode <src> <dst> <keyHex>".to_string(),
        ));
    }
    let key = i64::from_str_radix(args[2].trim_start_matches("0x"), 16)
        .map(|v| v as i32)
        .map_err(|e| Error::IllegalArgument(format!("bad key: {e}")))?;
    let src = std::path::Path::new(&args[0]);
    let dst = std::path::Path::new(&args[1]);
    // the Java handler writes into an existing empty destination
    let _ = std::fs::remove_file(dst);
    std::fs::File::create(dst)?;
    JetEncodingHandler::add_encoding_with_key(src, dst, key)?;
    out("encoded", dst.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// datefmt
// ---------------------------------------------------------------------------

/// The fixed values both sides format and parse (see `DbApiGen.datefmt`).
fn datefmt_values() -> Vec<(&'static str, LocalDateTime)> {
    vec![
        (
            "pre-gregorian",
            LocalDateTime::new(1582, 10, 4, 12, 34, 56, 0),
        ),
        (
            "gregorian-switch",
            LocalDateTime::new(1582, 10, 15, 0, 0, 0, 0),
        ),
        (
            "modern",
            LocalDateTime::new(2024, 2, 29, 1, 2, 3, 400_000_000),
        ),
        ("epoch", LocalDateTime::new(1970, 1, 1, 0, 0, 0, 0)),
        ("max", LocalDateTime::new(9999, 12, 31, 23, 59, 59, 0)),
    ]
}

fn cmd_datefmt(args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Err(Error::IllegalArgument(
            "usage: dbapi datefmt <pattern>".to_string(),
        ));
    }
    let format = create_date_format(&args[0]);
    out("pattern", format.pattern());
    for (name, value) in datefmt_values() {
        let text = format.format(value);
        out(&format!("format.{}", name), &text);
        match format.parse(&text) {
            Ok(parsed) => out(
                &format!("parse.{}", name),
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
                    parsed.year,
                    parsed.month,
                    parsed.day,
                    parsed.hour,
                    parsed.minute,
                    parsed.second,
                    parsed.nano / 1_000_000
                ),
            ),
            Err(_) => out(&format!("parse.{}", name), "ParseException"),
        }
    }
    Ok(())
}
