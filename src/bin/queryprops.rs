//! `queryprops` - dumps the property maps and name map of every query of a
//! database in the canonical text form `java-src/QueryPropsGen.java` also
//! writes.
//!
//! Usage:  queryprops <db> <out>
//!
//! The dump is the verification artifact of `tools/verify-queryprops.ps1`: the
//! Java tool and this binary read the same database (an Access-created
//! reference file or one the Java tool created with the public property API)
//! and must produce byte-identical text.  Every record is one line; string
//! values are escaped (`\\`, `\t`, `\r`, `\n`, `\0`) and `<null>` marks a
//! missing value.
//!
//! Records (see [`jackcess::query::dump_query_properties`] for the details):
//!
//! * `QUERY|...`, `STORAGE|...`, `PROPS|...`, `MAP|...`, `PROP|...`: the
//!   catalog row id of the query, the raw `LvProp` bytes and the decoded
//!   property maps (sorted by group name and property name).
//! * `NM|...`, `NMREC|...`: the name map stored in the query's `DOL` property.
//! * `PROPSERROR|...` / `NMERROR|...`: a stored value which cannot be decoded
//!   (the Java `IOException` of `getProperties()` / `getNameMap()`).
//! * `FATAL|...`: a failure which stopped the dump.

use std::fmt::Write as _;

use jackcess::database::Database;
use jackcess::error::Error;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: queryprops <db> <out>");
        std::process::exit(2);
    }

    let mut out = String::from("JACKCESS-QUERYPROPS 1\n");
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
            if let Err(error) = dump(&mut db, &mut out) {
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

fn dump(db: &mut Database, out: &mut String) -> jackcess::Result<()> {
    // the raw `LvProp` values, read once for the whole catalog
    let blobs = db.get_object_property_blobs()?;
    // the db-bound `Database.getQueries()`: every query knows the database
    let mut queries = db.get_queries()?;
    for query in &mut queries {
        // Java reads the group lazily; the port reads it here and keeps a
        // decoding error on the query, where `get_properties` reports it
        let _ = db.load_query_properties(query);
        let storage = blobs
            .get(&query.object_id())
            .and_then(|blob| blob.as_deref());
        jackcess::query::dump_query_properties(query, storage, out);
    }
    Ok(())
}

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
