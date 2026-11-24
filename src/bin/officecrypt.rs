//! `officecrypt` - the Rust counterpart of `java-src/OfficeCryptGen.java`.
//!
//! It creates the same plain V2016 / V2007 `.accdb` bases, the same agile
//! encrypted variants (OFFICE_2010 and OFFICE_2013), decrypts them back and
//! dumps the rows read through the crypt codec provider, so
//! `tools/verify-crypt-office.ps1` can compare the two implementations.
//!
//! Modes:
//!   gen     <outdir> [--password P] [--now-ms N] [--now-ms2 M]
//!   read    <dbfile> <password|-> <out.txt>
//!   decrypt <src> <dst> <password>
//!   checkpw <dbfile> <password>
//!   fixed   <src> <dst> <password> <2010|2013>

use jackcess::column::ColumnSpec;
use jackcess::crypt::{
    add_office_encryption_with_key_material, remove_office_encryption, AgileEncryptionConfig,
    AgileKeyMaterial,
};
use jackcess::database::Database;
use jackcess::datatype::DataType;
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;
use jackcess::{Error, Result};
use std::path::{Path, PathBuf};

const TABLE: &str = "Test";
const DEFAULT_PASSWORD: &str = "s3cret-Pa55";
const FIXED_ENCODING_KEY: i32 = 0x0BAD_F00D;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        std::process::exit(2);
    }
    let result = match args[1].as_str() {
        "gen" => {
            if args.len() < 3 {
                usage();
                std::process::exit(2);
            }
            let out_dir = PathBuf::from(&args[2]);
            let mut password = DEFAULT_PASSWORD.to_string();
            let mut now_ms: Option<i64> = None;
            let mut now_ms2: Option<i64> = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--password" => {
                        if let Some(v) = args.get(i + 1) {
                            password = v.clone();
                        }
                        i += 2;
                    }
                    "--now-ms" => {
                        now_ms = args.get(i + 1).and_then(|v| v.trim().parse::<i64>().ok());
                        i += 2;
                    }
                    "--now-ms2" => {
                        now_ms2 = args.get(i + 1).and_then(|v| v.trim().parse::<i64>().ok());
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            gen(&out_dir, &password, now_ms, now_ms2)
        }
        "read" => {
            if args.len() < 5 {
                usage();
                std::process::exit(2);
            }
            let db_file = PathBuf::from(&args[2]);
            let password = if args[3] == "-" {
                None
            } else {
                Some(args[3].as_str())
            };
            let out = PathBuf::from(&args[4]);
            dump_rows(&db_file, password).and_then(|dump| {
                std::fs::write(&out, dump.as_bytes())?;
                println!("wrote {}", out.display());
                Ok(())
            })
        }
        "decrypt" => {
            if args.len() < 5 {
                usage();
                std::process::exit(2);
            }
            remove_office_encryption(Path::new(&args[2]), Path::new(&args[3]), &args[4])
        }
        "checkpw" => {
            if args.len() < 4 {
                usage();
                std::process::exit(2);
            }
            check_password(Path::new(&args[2]), &args[3])
        }
        "add" => {
            if args.len() < 5 {
                usage();
                std::process::exit(2);
            }
            let _ = std::fs::remove_file(&args[3]);
            jackcess::crypt::add_office_encryption(
                Path::new(&args[2]),
                Path::new(&args[3]),
                &args[4],
            )
        }
        "fixed" => {
            if args.len() < 6 {
                usage();
                std::process::exit(2);
            }
            fixed(Path::new(&args[2]), Path::new(&args[3]), &args[4], &args[5])
        }
        "encrypt-in-place" => {
            if args.len() < 4 {
                usage();
                std::process::exit(2);
            }
            if args.get(4).map(String::as_str) == Some("2013") {
                jackcess::crypt::add_office_encryption_in_place_with_config(
                    Path::new(&args[2]),
                    &args[3],
                    &AgileEncryptionConfig::office_2013(),
                )
            } else {
                jackcess::crypt::add_office_encryption_in_place(Path::new(&args[2]), &args[3])
            }
        }
        "decrypt-in-place" => {
            if args.len() < 4 {
                usage();
                std::process::exit(2);
            }
            jackcess::crypt::remove_office_encryption_in_place(Path::new(&args[2]), &args[3])
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        if let Error::InvalidCredentials(_) = e {
            eprintln!("InvalidCredentialsException: {}", e);
            std::process::exit(3);
        }
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn usage() {
    eprintln!("usage: officecrypt gen <outdir> [--password P] [--now-ms N] [--now-ms2 M]");
    eprintln!("       officecrypt read <dbfile> <password|-> <out.txt>");
    eprintln!("       officecrypt decrypt <src> <dst> <password>");
    eprintln!("       officecrypt checkpw <dbfile> <password>");
    eprintln!("       officecrypt add <src> <dst> <password>");
    eprintln!("       officecrypt fixed <src> <dst> <password> <2010|2013>");
    eprintln!("       officecrypt encrypt-in-place <file> <password> [2013]");
    eprintln!("       officecrypt decrypt-in-place <file> <password>");
}

/// The whole `OfficeCryptGen gen` work: the two plain bases and every encrypted
/// / decrypted variant, plus the row dumps and file hashes.
fn gen(out_dir: &Path, password: &str, now_ms: Option<i64>, now_ms2: Option<i64>) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;

    let base2016 = out_dir.join("base2016.accdb");
    let _ = std::fs::remove_file(&base2016);
    create_base(&base2016, FileFormat::V2016, now_ms)?;

    let base2007 = out_dir.join("base2007.accdb");
    let _ = std::fs::remove_file(&base2007);
    create_base(&base2007, FileFormat::V2007, now_ms2)?;

    let agile2016 = out_dir.join("agile2016.accdb");
    let _ = std::fs::remove_file(&agile2016);
    jackcess::crypt::add_office_encryption(&base2016, &agile2016, password)?;

    let agile2013 = out_dir.join("agile2013.accdb");
    let _ = std::fs::remove_file(&agile2013);
    jackcess::crypt::add_office_encryption_with_config(
        &base2016,
        &agile2013,
        password,
        &AgileEncryptionConfig::office_2013(),
    )?;

    let decrypted2016 = out_dir.join("decrypted2016.accdb");
    let _ = std::fs::remove_file(&decrypted2016);
    remove_office_encryption(&agile2016, &decrypted2016, password)?;

    let agile2007 = out_dir.join("agile2007.accdb");
    let _ = std::fs::remove_file(&agile2007);
    jackcess::crypt::add_office_encryption(&base2007, &agile2007, password)?;

    let decrypted2007 = out_dir.join("decrypted2007.accdb");
    let _ = std::fs::remove_file(&decrypted2007);
    remove_office_encryption(&agile2007, &decrypted2007, password)?;

    // rows read back through the codec provider, and the SHA-256 of every file
    let mut hashes = String::new();
    for name in [
        "base2016.accdb",
        "agile2016.accdb",
        "agile2013.accdb",
        "decrypted2016.accdb",
        "base2007.accdb",
        "agile2007.accdb",
        "decrypted2007.accdb",
    ] {
        let f = out_dir.join(name);
        let stem = name.trim_end_matches(".accdb");
        let dump = dump_rows(&f, Some(password))?;
        std::fs::write(out_dir.join(format!("{}.rows.txt", stem)), dump.as_bytes())?;
        hashes.push_str(&format!("{} {}\n", stem, sha256_hex(&std::fs::read(&f)?)?));
    }
    std::fs::write(out_dir.join("hashes.txt"), hashes.as_bytes())?;

    // a wrong password is refused by the codec (InvalidCredentialsException)
    let wrong =
        match Database::open_path_with_password(&agile2016, Some(&format!("wrong-{}", password))) {
            Ok(mut db) => format!(
                "opened password={}\n",
                fmt_password(db.get_database_password()?)
            ),
            Err(e) => format!("InvalidCredentialsException: {}\n", e),
        };
    std::fs::write(out_dir.join("wrongpassword.txt"), wrong.as_bytes())?;

    println!("wrote {}", out_dir.display());
    Ok(())
}

/// The deterministic `fixed` mode: the key material and the page 0 encoding key
/// the Java `OfficeCryptGen fixed` mode uses.
fn fixed(src: &Path, dst: &Path, password: &str, profile: &str) -> Result<()> {
    let config = if profile == "2013" {
        AgileEncryptionConfig::office_2013()
    } else {
        AgileEncryptionConfig::office_2010()
    };
    let material = AgileKeyMaterial {
        key_data_salt: seq(0x00, 16),
        pwd_salt: seq(0x10, 16),
        key_value: seq(0x20, config.key_byte_len()),
        verifier_input: seq(0x30, 16),
    };
    let _ = std::fs::remove_file(dst);
    add_office_encryption_with_key_material(
        src,
        dst,
        password,
        &config,
        &material,
        FIXED_ENCODING_KEY,
    )
}

fn seq(start: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| start.wrapping_add(i as u8)).collect()
}

/// The "Test" table of `OfficeCryptGen.createBase` / `Gen.java`'s "rows"
/// scenario.
fn create_base(base: &Path, format: FileFormat, now_ms: Option<i64>) -> Result<()> {
    let mut db = Database::create(format, base)?;
    db.now_ms = now_ms;
    let spec = TableSpec::new(
        TABLE,
        vec![
            ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", DataType::Text).with_length(50),
            ColumnSpec::new("Value", DataType::Double),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    for i in 0..5 {
        db.add_rows(
            TABLE,
            &[vec![
                Value::Null,
                Value::Text(format!("name{}", i)),
                Value::Double(1.5 * i as f64),
            ]],
        )?;
    }
    db.flush()?;
    Ok(())
}

fn open_db(db_file: &Path, password: Option<&str>) -> Result<Database> {
    Database::open_path_with_password(db_file, password)
}

fn check_password(db_file: &Path, password: &str) -> Result<()> {
    match open_db(db_file, Some(password)) {
        Ok(mut db) => {
            println!(
                "opened password={}",
                fmt_password(db.get_database_password()?)
            );
            println!("PASSWORD-ACCEPTED");
            Ok(())
        }
        Err(Error::InvalidCredentials(message)) => {
            println!("InvalidCredentialsException: {}", message);
            println!("PASSWORD-REJECTED");
            std::process::exit(3);
        }
        Err(e) => Err(e),
    }
}

/// `OfficeCryptGen.dumpRows`: the rows of the test table and the two SID
/// tables, plus the stored page 0 password field escaped by UTF-16 unit.
fn dump_rows(db_file: &Path, password: Option<&str>) -> Result<String> {
    let mut db = open_db(db_file, password)?;
    let mut out = String::new();
    out.push_str(&format!(
        "password={}\n",
        fmt_password(db.get_database_password()?)
    ));

    let indexes = {
        let t = db.get_table(TABLE)?;
        (
            t.column_index("ID"),
            t.column_index("Name"),
            t.column_index("Value"),
        )
    };
    out.push_str(&format!("table {} rows:\n", TABLE));
    for (_, values) in db.read_rows_with_ids(TABLE)? {
        let Some(values) = values else {
            continue;
        };
        out.push_str(&get_value(&values, indexes.0));
        out.push('\t');
        out.push_str(&get_value(&values, indexes.1));
        out.push('\t');
        out.push_str(&get_value(&values, indexes.2));
        out.push('\n');
    }

    for (sys_name, col) in [("MSysACEs", "SID"), ("MSysObjects", "Owner")] {
        let col_index = {
            match db.find_table(sys_name)? {
                Some(t) => t.column_index(col),
                None => {
                    out.push_str(&format!("table {} missing\n", sys_name));
                    continue;
                }
            }
        };
        out.push_str(&format!("table {}.{} rows:\n", sys_name, col));
        for (_, values) in db.read_rows_with_ids(sys_name)? {
            let Some(values) = values else {
                continue;
            };
            out.push_str(&get_value(&values, col_index));
            out.push('\n');
        }
    }

    Ok(out)
}

fn get_value(values: &[Value], index: Option<usize>) -> String {
    match index.and_then(|i| values.get(i)) {
        None => fmt_value(None),
        Some(v) => fmt_value(Some(v)),
    }
}

/// The stored password field, escaped by UTF-16 unit so any byte survives.
fn fmt_password(password: Option<String>) -> String {
    match password {
        None => "null".to_string(),
        Some(text) => {
            let mut s = String::new();
            for unit in text.encode_utf16() {
                s.push_str(&format!("\\u{:04x}", unit));
            }
            s
        }
    }
}

/// The exact textual form of a value, byte for byte what
/// `OfficeCryptGen.fmt` writes.
fn fmt_value(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "n:".to_string(),
        Some(Value::Bytes(b)) => {
            let mut s = String::from("b:");
            for byte in b {
                s.push_str(&format!("{:02x}", byte));
            }
            s
        }
        Some(Value::Text(text)) => {
            let mut s = String::from("s:");
            for c in text.chars() {
                match c {
                    '\\' => s.push_str("\\u005c"),
                    '\t' => s.push_str("\\u0009"),
                    '\n' => s.push_str("\\u000a"),
                    '\r' => s.push_str("\\u000d"),
                    _ => s.push(c),
                }
            }
            s
        }
        Some(Value::Double(d)) => format!("d:{:x}", d.to_bits()),
        Some(Value::Float(f)) => format!("d:{:x}", (*f as f64).to_bits()),
        Some(Value::Byte(i)) => format!("i:{}", i),
        Some(Value::Int(i)) => format!("i:{}", i),
        Some(Value::Long(i)) => format!("i:{}", i),
        Some(Value::BigInt(i)) => format!("i:{}", i),
        Some(Value::Bool(b)) => format!("z:{}", if *b { 1 } else { 0 }),
        Some(Value::Decimal { unscaled, scale }) => format!("x:{}E-{}", unscaled, scale),
        Some(Value::DateTime(dt)) => format!("x:{:?}", dt),
        Some(Value::DateBits(bits)) => format!("x:{:x}", bits.to_bits()),
        Some(Value::Uuid(u)) => {
            let mut s = String::from("x:");
            for byte in u {
                s.push_str(&format!("{:02x}", byte));
            }
            s
        }
        Some(Value::Keep) => "x:keep".to_string(),
    }
}

fn sha256_hex(data: &[u8]) -> Result<String> {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(data);
    Ok(hex::encode(digest.finalize()))
}
