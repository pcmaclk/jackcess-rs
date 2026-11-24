//! `querydb` - dumps the query definitions and the table metadata of a
//! database in the canonical text form `java-src/QueryDbGen.java` also writes.
//!
//! Usage:  querydb <file.accdb> <dump-out.txt> [sql-out.txt]
//!
//! The dump is the verification artifact of `tools/verify-querydb.ps1`: the
//! Java tool and this binary read the same Java-generated database and must
//! produce byte-identical text (the SQL embeds the platform newline, exactly
//! like `Query.toSQLString()`).

use std::fmt::Write as _;

use jackcess::database::{Database, TableMetaData, TableMetaDataType};
use jackcess::error::Error;
use jackcess::query::Query;

/// The tables whose metadata the dump describes, in dump order.  A missing
/// table is skipped (the corpus creates some tables only for the formats
/// which support them).
const METADATA_TABLES: &[&str] = &[
    "Data",
    "Child",
    "CalcData",
    "AppendData",
    "LinkedData",
    "OdbcData",
    "MSysQueries",
];

/// The tables whose created/updated dates, columns, indexes and row count are
/// dumped.  `MSysQueries` is a system table (read through the catalog lookup).
const DETAIL_TABLES: &[&str] = &[
    "Data",
    "Child",
    "CalcData",
    "AppendData",
    "OdbcData",
    "MSysQueries",
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: querydb <file.accdb> <dump-out.txt> [sql-out.txt]");
        std::process::exit(2);
    }
    let db_path = std::path::Path::new(&args[1]);
    let mut db = match Database::open_path(db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error opening {}: {}", args[1], e);
            std::process::exit(1);
        }
    };
    let (dump, sql_dump) = match build_dump(&mut db) {
        Ok(result) => result,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(1);
        }
    };
    if let Err(e) = std::fs::write(&args[2], dump) {
        eprintln!("error writing {}: {}", args[2], e);
        std::process::exit(1);
    }
    if let Some(sql_path) = args.get(3) {
        if let Err(e) = std::fs::write(sql_path, sql_dump) {
            eprintln!("error writing {}: {}", sql_path, e);
            std::process::exit(1);
        }
    }
}

fn build_dump(db: &mut Database) -> jackcess::Result<(String, String)> {
    let mut out = String::new();
    let mut sql_out = String::new();
    out.push_str("JACKCESS-QUERYDB 1\n");

    // ---- queries -----------------------------------------------------
    let queries = db.get_queries()?;
    for query in &queries {
        append_query(&mut out, query)?;
        let _ = writeln!(sql_out, "{}\t{}", query.name(), query.to_sql_string()?);
    }

    // ---- table metadata ---------------------------------------------
    for name in METADATA_TABLES {
        let meta = match db.get_table_meta_data(name)? {
            Some(meta) => meta,
            None => continue,
        };
        append_table_metadata(&mut out, &meta);
    }

    // ---- per table details ------------------------------------------
    for name in DETAIL_TABLES {
        dump_table_details(db, &mut out, name)?;
    }

    // ---- foreign key lookup (Child -> Data) -------------------------
    dump_foreign_key(db, &mut out, "Child", "Data")?;

    // ---- definition-only (type 4) row access ------------------------
    if let Some(meta) = db.get_table_meta_data("OdbcData")? {
        // opening the table works (it is a definition), reading a row does not
        let page = {
            let table = db.get_table("OdbcData")?;
            table.table_def_page_number
        };
        let error = match db.read_table_rows(page) {
            Ok(_) => "NONE".to_string(),
            Err(e) => exception_message(&e),
        };
        let _ = writeln!(out, "OPEN|{}|error={}", meta.name, error);
    }

    Ok((out, sql_out))
}

fn append_query(out: &mut String, query: &Query) -> jackcess::Result<()> {
    let parameters = query.get_parameters()?;
    let _ = writeln!(
        out,
        "QUERY|{}|{}|objectFlag={}|objectId={}|hidden={}|ownerAccess={}|parameters={}",
        query.name(),
        query.query_type().name(),
        query.object_flag(),
        query.object_id(),
        query.is_hidden(),
        query.get_owner_access_type()?,
        parameters.join("|")
    );
    let _ = writeln!(out, "SQL|{}", query.to_sql_string()?);
    Ok(())
}

fn append_table_metadata(out: &mut String, meta: &TableMetaData) {
    let _ = writeln!(
        out,
        "TABLE|{}|type={}|linked={}|system={}|linkedTable={}|linkedDb={}|connection={}|created={}|updated={}|page={}",
        meta.name,
        meta.table_type.name(),
        meta.is_linked(),
        meta.is_system(),
        meta.linked_table_name.as_deref().unwrap_or(""),
        meta.linked_db_name.as_deref().unwrap_or(""),
        meta.connection_name.as_deref().unwrap_or(""),
        format_date(meta.created_date),
        format_date(meta.updated_date),
        meta.page_number
            .map(|page| page.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    debug_assert!(matches!(
        meta.table_type,
        TableMetaDataType::Local | TableMetaDataType::Linked | TableMetaDataType::LinkedOdbc
    ));
}

fn dump_table_details(db: &mut Database, out: &mut String, name: &str) -> jackcess::Result<()> {
    let page = {
        let table = match db.find_table(name)? {
            Some(table) => table,
            None => return Ok(()),
        };
        table.table_def_page_number
    };
    let columns = db.get_column_meta_data_all(name)?;
    {
        let table = &db.tables[&page];
        let _ = writeln!(
            out,
            "DATE|{}|created={}|updated={}",
            table.name,
            format_date(table.get_created_date()),
            format_date(table.get_updated_date())
        );
        for column in columns {
            let _ = writeln!(
                out,
                "COLUMN|{}|{}|{}|sqlType={}|length={}|units={}|precision={}|scale={}|autoNumber={}|hidden={}|calculated={}|appendOnly={}|variable={}",
                table.name,
                column.name,
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
            let meta = table.index_meta_data(index);
            let columns = meta
                .columns
                .iter()
                .zip(meta.ascending.iter())
                .map(|(name, ascending)| format!("{}:{}", name, ascending))
                .collect::<Vec<_>>()
                .join(",");
            let _ = writeln!(
                out,
                "INDEX|{}|{}|primary={}|foreign={}|unique={}|required={}|ignoreNulls={}|columns={}",
                table.name,
                meta.name,
                meta.primary_key,
                meta.foreign_key,
                meta.unique,
                meta.required,
                meta.ignore_nulls,
                columns
            );
        }
        let _ = writeln!(out, "ROWS|{}|{}", table.name, table.get_row_count());
    }
    Ok(())
}

fn dump_foreign_key(
    db: &mut Database,
    out: &mut String,
    table_name: &str,
    other_name: &str,
) -> jackcess::Result<()> {
    let table_page = match db.find_table(table_name)? {
        Some(table) => table.table_def_page_number,
        None => return Ok(()),
    };
    let other_page = match db.find_table(other_name)? {
        Some(table) => table.table_def_page_number,
        None => return Ok(()),
    };
    let index_name = {
        let table = &db.tables[&table_page];
        let other = &db.tables[&other_page];
        match table.get_foreign_key_index(other) {
            Ok(index) => index.name.clone(),
            Err(_) => return Ok(()),
        }
    };
    let _ = writeln!(
        out,
        "FK|{}|{}|{}",
        db.tables[&table_page].name, db.tables[&other_page].name, index_name
    );
    Ok(())
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

/// The Java `Throwable.getMessage()` of the Rust error (the prefix
/// [`Error`]'s `Display` adds for its kind is not part of the Java message).
fn exception_message(error: &Error) -> String {
    match error {
        Error::Io(e) => e.to_string(),
        Error::Jackcess(message)
        | Error::IllegalArgument(message)
        | Error::IllegalState(message)
        | Error::Unsupported(message)
        | Error::ConstraintViolation(message)
        | Error::InvalidValue(message)
        | Error::BufferUnderflow(message)
        | Error::BufferOverflow(message) => message.clone(),
        other => other.to_string(),
    }
}
