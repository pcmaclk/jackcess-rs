//! `oledump` - dumps Access OLE column values and raw OLE compound files in a
//! canonical text form which is byte-comparable with the Java reference dump
//! produced by `java-src/OleDumpGen.java`.
//!
//! Modes:
//! * `db <database> <out>` - every OLE column value of every table;
//! * `blob <dir> <out>` - every `*.bin` file in `<dir>` parsed as an Access
//!   OLE value;
//! * `compound <dir> <out>` - every `*.bin` file in `<dir>` parsed as a raw
//!   OLE2 compound document.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::error::{Error, Result};
use jackcess::ole::{ContentType, OleBlob};
use jackcess::value::Value;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 4 {
        eprintln!("usage: oledump db|blob|compound <input> <out>");
        std::process::exit(2);
    }
    if let Err(e) = run(&args[1], &args[2], &args[3]) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(mode: &str, input: &str, out: &str) -> Result<()> {
    let file = File::create(out)?;
    let mut w = BufWriter::new(file);
    match mode {
        "db" => dump_db(input, &mut w)?,
        "blob" => dump_dir(input, &mut w, Mode::Blob)?,
        "compound" => dump_dir(input, &mut w, Mode::Compound)?,
        other => {
            return Err(Error::IllegalArgument(format!("unknown mode {other}")));
        }
    }
    w.flush()?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Blob,
    Compound,
}

fn dump_db(path: &str, w: &mut impl Write) -> Result<()> {
    let mut db = Database::open_path(Path::new(path))?;
    writeln!(w, "# oledump v1 mode=db")?;
    let file_name = Path::new(path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    writeln!(w, "db \"{}\"", escape(&file_name))?;

    let mut table_names = db.get_table_names()?;
    table_names.sort();
    for table_name in table_names {
        let (ole_columns, row_count) = {
            let table = match db.find_table(&table_name)? {
                Some(table) => table,
                None => continue,
            };
            let columns: Vec<(usize, String)> = table
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.data_type == DataType::Ole)
                .map(|(i, c)| (i, c.name.clone()))
                .collect();
            (columns, table.row_count)
        };
        if ole_columns.is_empty() {
            continue;
        }
        writeln!(w, "table \"{}\" rows={}", escape(&table_name), row_count)?;
        for (_, name) in &ole_columns {
            writeln!(
                w,
                "col \"{}\" \"{}\" OLE",
                escape(&table_name),
                escape(name)
            )?;
        }

        let rows = db.read_rows_with_ids(&table_name)?;
        let mut display_row = 0;
        for (_, row) in rows {
            let Some(row) = row else { continue };
            for (index, column_name) in &ole_columns {
                let ctx = format!(
                    "\"{}\"/{}/\"{}\"",
                    escape(&table_name),
                    display_row,
                    escape(column_name)
                );
                match row.get(*index) {
                    Some(Value::Null) | None => {
                        writeln!(w, "value {} null", ctx)?;
                    }
                    Some(Value::Bytes(bytes)) => {
                        dump_blob_value(w, &ctx, bytes.as_slice())?;
                    }
                    Some(other) => {
                        writeln!(
                            w,
                            "value {} unsupported={:?}",
                            ctx,
                            std::mem::discriminant(other)
                        )?;
                    }
                }
            }
            display_row += 1;
        }
    }
    Ok(())
}

fn dump_dir(dir: &str, w: &mut impl Write, mode: Mode) -> Result<()> {
    writeln!(
        w,
        "# oledump v1 mode={}",
        if mode == Mode::Blob {
            "blob"
        } else {
            "compound"
        }
    )?;
    let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "bin").unwrap_or(false))
        .collect();
    files.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .cmp(&b.file_name().unwrap_or_default().to_string_lossy())
    });
    for file in files {
        let name = file
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let bytes = std::fs::read(&file)?;
        let ctx = format!("\"{}\"", escape(&name));
        match mode {
            Mode::Blob => dump_blob_value(w, &ctx, &bytes)?,
            Mode::Compound => {
                writeln!(w, "value {} len={}", ctx, bytes.len())?;
                write_compound_section_body(w, &bytes)?;
            }
        }
    }
    Ok(())
}

/// Dumps one Access OLE value (`OleBlob`).
fn dump_blob_value(w: &mut impl Write, ctx: &str, bytes: &[u8]) -> Result<()> {
    writeln!(w, "value {} len={}", ctx, bytes.len())?;
    let blob = OleBlob::from_internal_data(bytes);
    let content = match blob.content() {
        Ok(content) => content,
        Err(err) => {
            let (class, message) = classify_error(&err);
            writeln!(w, "  contentError {} {}", class, message)?;
            return Ok(());
        }
    };
    let content_type = content.content_type();
    writeln!(w, "  type {}", content_type.name())?;
    if content_type == ContentType::Unknown {
        return Ok(());
    }

    writeln!(
        w,
        "  pretty \"{}\"",
        escape(content.pretty_name().unwrap_or(""))
    )?;
    writeln!(
        w,
        "  class \"{}\"",
        escape(content.class_name().unwrap_or(""))
    )?;
    writeln!(
        w,
        "  typeName \"{}\"",
        escape(content.type_name().unwrap_or(""))
    )?;
    write_optional_str(w, "fileName", content.file_name())?;
    write_optional_str(w, "filePath", content.file_path())?;
    write_optional_str(w, "localFilePath", content.local_file_path())?;
    write_optional_str(w, "linkPath", content.link_path())?;
    writeln!(w, "  contentLength {}", content.length())?;

    if content_type == ContentType::Link {
        // `LinkContent` has no embedded content (Java's length is -1).
        writeln!(w, "  contentSha256 -")?;
    } else {
        match content.data() {
            Ok(data) => {
                writeln!(w, "  contentSha256 {}", sha256_hex(data))?;
            }
            Err(err) => {
                let (class, message) = classify_error(&err);
                writeln!(w, "  contentError {} {}", class, message)?;
            }
        }
    }
    if let Some(compound) = content.as_compound() {
        match compound.data() {
            Ok(data) => {
                writeln!(w, "  compound")?;
                write_compound_section_body(w, data)?;
                write_compound_api(w, compound)?;
            }
            Err(err) => {
                let (class, message) = classify_error(&err);
                writeln!(w, "  compound")?;
                writeln!(w, "    dataError {} {}", class, message)?;
            }
        }
    }
    Ok(())
}

fn write_optional_str(w: &mut impl Write, field: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => writeln!(w, "  {} \"{}\"", field, escape(value))?,
        None => writeln!(w, "  {} null", field)?,
    }
    Ok(())
}

fn write_compound_section_body(w: &mut impl Write, bytes: &[u8]) -> Result<()> {
    let header = match jackcess::ole::CompoundHeader::parse(bytes) {
        Ok(header) => header,
        Err(err) => {
            let (class, message) = classify_error(&err);
            writeln!(w, "    headerError {} {}", class, message)?;
            return Ok(());
        }
    };
    let difat: Vec<String> = header.difat.iter().map(|v| v.to_string()).collect();
    writeln!(
        w,
        "    header sectorShift={} miniSectorShift={} minor={} major={} \
         byteOrder={} numDirSectors={} numFatSectors={} firstDirSector={} \
         txn={} miniCutoff={} firstMiniFat={} numMiniFat={} firstDifat={} \
         numDifat={}",
        header.sector_shift,
        header.mini_sector_shift,
        header.minor_version,
        header.major_version,
        header.byte_order,
        header.num_dir_sectors,
        header.num_fat_sectors,
        header.first_dir_sector,
        header.transaction_signature,
        header.mini_stream_cutoff_size,
        header.first_mini_fat_sector,
        header.num_mini_fat_sectors,
        header.first_difat_sector,
        header.num_difat_sectors
    )?;
    writeln!(w, "    hclsid {}", hex_lower(&header.clsid))?;
    writeln!(w, "    difat {}", difat.join(","))?;

    let file = match jackcess::ole::CompoundFile::parse(bytes) {
        Ok(file) => file,
        Err(err) => {
            let (class, message) = classify_error(&err);
            writeln!(w, "    openError {} {}", class, message)?;
            return Ok(());
        }
    };
    writeln!(w, "    open ok")?;

    let root = file.root();
    writeln!(
        w,
        "    root clsid={} size={} ctime={:016x} mtime={:016x}",
        root.clsid_string(),
        root.size32(),
        root.creation_time(),
        root.modified_time()
    )?;

    // Collect all descendants of the root, sorted by their raw path.
    let mut entries: Vec<(String, usize)> = Vec::new();
    collect_raw_entries(&file, 0, "", &mut entries);
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (path, index) in entries {
        let entry = &file.directory_entries()[index];
        let kind = if entry.is_storage() {
            "storage"
        } else {
            "stream"
        };
        if entry.is_storage() {
            writeln!(
                w,
                "    entry \"{}\" kind={} size={} ctime={:016x} mtime={:016x} clsid={}",
                escape(&path),
                kind,
                entry.size32(),
                entry.creation_time(),
                entry.modified_time(),
                entry.clsid_string()
            )?;
        } else {
            match file.read_stream(index) {
                Ok(data) => {
                    writeln!(
                        w,
                        "    entry \"{}\" kind={} size={} ctime={:016x} mtime={:016x} \
                         sha256={}",
                        escape(&path),
                        kind,
                        entry.size32(),
                        entry.creation_time(),
                        entry.modified_time(),
                        sha256_hex(&data)
                    )?;
                }
                Err(err) => {
                    let (class, message) = classify_error(&err);
                    writeln!(
                        w,
                        "    entry \"{}\" kind={} size={} ctime={:016x} mtime={:016x} \
                         error={} {}",
                        escape(&path),
                        kind,
                        entry.size32(),
                        entry.creation_time(),
                        entry.modified_time(),
                        class,
                        message
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn collect_raw_entries(
    file: &jackcess::ole::CompoundFile,
    index: usize,
    prefix: &str,
    out: &mut Vec<(String, usize)>,
) {
    for &child in file.children_of(index) {
        let entry = &file.directory_entries()[child];
        if entry.entry_type() == jackcess::ole::EntryType::Storage {
            let path = format!("{}/{}", prefix, entry.name());
            out.push((path.clone(), child));
            collect_raw_entries(file, child, &path, out);
        } else if entry.is_stream() {
            out.push((format!("{}/{}", prefix, entry.name()), child));
        }
    }
}

/// Dumps the Jackcess `CompoundContent` API view (encoded entry names).
fn write_compound_api(
    w: &mut impl Write,
    compound: &jackcess::ole::CompoundContent<'_>,
) -> Result<()> {
    // The generic API only exists if the compound file opens; if it does not,
    // the compound section above already reports the exact error.
    if compound.compound_file().is_err() {
        writeln!(w, "  api skipped=compoundError")?;
        return Ok(());
    }
    match compound.has_contents_entry() {
        Ok(value) => writeln!(w, "  api hasContentsEntry={}", value)?,
        Err(err) => {
            let (class, message) = classify_error(&err);
            writeln!(w, "  api hasContentsEntryError {} {}", class, message)?;
        }
    }
    if compound.has_contents_entry().unwrap_or(false) {
        match compound.contents_entry() {
            Ok(entry) => match entry.data() {
                Ok(data) => writeln!(
                    w,
                    "  api contentsEntry \"{}\" size={} sha256={}",
                    escape(entry.name()),
                    entry.length(),
                    sha256_hex(&data)
                )?,
                Err(err) => {
                    let (class, message) = classify_error(&err);
                    writeln!(
                        w,
                        "  api contentsEntry \"{}\" size={} error={} {}",
                        escape(entry.name()),
                        entry.length(),
                        class,
                        message
                    )?;
                }
            },
            Err(err) => {
                let (class, message) = classify_error(&err);
                writeln!(w, "  api contentsEntryError {} {}", class, message)?;
            }
        }
    }

    match compound.entries() {
        Ok(mut entries) => {
            entries.sort_by(|a, b| a.name().cmp(b.name()));
            for entry in entries {
                match entry.data() {
                    Ok(data) => writeln!(
                        w,
                        "  api entry \"{}\" size={} sha256={}",
                        escape(entry.name()),
                        entry.length(),
                        sha256_hex(&data)
                    )?,
                    Err(err) => {
                        let (class, message) = classify_error(&err);
                        writeln!(
                            w,
                            "  api entry \"{}\" size={} error={} {}",
                            escape(entry.name()),
                            entry.length(),
                            class,
                            message
                        )?;
                    }
                }
            }
        }
        Err(err) => {
            let (class, message) = classify_error(&err);
            writeln!(w, "  api entriesError {} {}", class, message)?;
        }
    }
    Ok(())
}

/// Escapes a string the same way the Java harness does: printable ASCII is
/// kept, non-ASCII is emitted as one `\uXXXX` per UTF-16 code unit.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for unit in value.encode_utf16() {
        match unit {
            0x5C => out.push_str("\\\\"),
            0x22 => out.push_str("\\\""),
            0x0A => out.push_str("\\n"),
            0x0D => out.push_str("\\r"),
            0x09 => out.push_str("\\t"),
            0x20..=0x7E => out.push(unit as u8 as char),
            _ => out.push_str(&format!("\\u{:04X}", unit)),
        }
    }
    out
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_lower(&hasher.finalize())
}

/// Maps an error to Java's exception class name and message (as printed by the
/// Java reference harness).
fn classify_error(err: &Error) -> (&'static str, String) {
    match err {
        Error::BufferUnderflow(_) => ("BufferUnderflowException", "<null>".to_string()),
        Error::IllegalArgument(message) => ("IllegalArgumentException", message.clone()),
        Error::IllegalState(message) => ("IllegalStateException", message.clone()),
        Error::Jackcess(message) => classify_message(message),
        other => ("OtherException", other.to_string()),
    }
}

fn classify_message(message: &str) -> (&'static str, String) {
    if message.starts_with("Invalid header signature") {
        ("NotOLE2FileException", message.to_string())
    } else if message.starts_with("Block ") && message.ends_with(" not found") {
        ("IndexOutOfBoundsException", message.to_string())
    } else if message.starts_with("Index -") && message.contains("out of bounds") {
        // POI's `BlockStore.ChainLoopDetector` keeps a `boolean[]` indexed by
        // the chain block number, so a negative block number surfaces as an
        // `ArrayIndexOutOfBoundsException` (with the same message).
        ("ArrayIndexOutOfBoundsException", message.to_string())
    } else if message.starts_with("Index ") && message.contains("out of bounds") {
        ("IndexOutOfBoundsException", message.to_string())
    } else if message.starts_with("Potential loop detected") {
        ("IllegalStateException", message.to_string())
    } else if message.starts_with("Illegal block count")
        || message.starts_with("Block count")
        || message.starts_with("Unsupported blocksize")
        || message.starts_with("Unable to read entire header")
    {
        ("IOException", message.to_string())
    } else if message.starts_with("newLimit >") || message.starts_with("newPosition") {
        ("IllegalArgumentException", message.to_string())
    } else if message.starts_with("String index out of range") || message.starts_with("Range [") {
        ("StringIndexOutOfBoundsException", message.to_string())
    } else if message.starts_with("Could not find document") || message.starts_with("no such entry")
    {
        ("FileNotFoundException", message.to_string())
    } else if message.starts_with("Can't read past the end of the stream") {
        ("NoSuchElementException", message.to_string())
    } else if message.starts_with("Can't read from a new stream") {
        ("IllegalStateException", message.to_string())
    } else {
        ("IOException", message.to_string())
    }
}
