//! `jetcrypt` - the Rust counterpart of `java-src/JetCryptGen.java`.
//!
//! It creates the same plain "Test" database (the `Gen.java` "rows" scenario)
//! and the same jet-encoded / database-password protected variants, and dumps
//! the rows read back through the crypt codec provider, so `tools/verify-crypt.ps1`
//! can compare the two implementations byte for byte.
//!
//! Modes:
//!   gen    <outdir> [--password P] [--key HEX] [--now-ms N]
//!   read   <dbfile> <password|-> <out.txt>
//!   checkpw <dbfile> <password>

use jackcess::column::ColumnSpec;
use jackcess::crypt::{decode_file, encode_file_with_key, PasswordUtil};
use jackcess::database::Database;
use jackcess::format::FileFormat;
use jackcess::tablecreator::{IndexSpec, TableSpec};
use jackcess::value::Value;
use jackcess::Result;
use std::path::{Path, PathBuf};

const TABLE: &str = "Test";
const DEFAULT_KEY: i32 = 0x1234_abcd;
const DEFAULT_PASSWORD: &str = "s3cret-Pa55";

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
            let mut key = DEFAULT_KEY;
            let mut now_ms: Option<i64> = None;
            let mut i = 3;
            while i < args.len() {
                match args[i].as_str() {
                    "--password" => {
                        if let Some(v) = args.get(i + 1) {
                            password = v.clone();
                        }
                        i += 2;
                    }
                    "--key" => {
                        if let Some(v) = args.get(i + 1) {
                            key = i64::from_str_radix(v.trim_start_matches("0x"), 16)
                                .map(|v| v as i32)
                                .unwrap_or(DEFAULT_KEY);
                        }
                        i += 2;
                    }
                    "--now-ms" => {
                        now_ms = args.get(i + 1).and_then(|v| v.trim().parse::<i64>().ok());
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            gen(&out_dir, &password, key, now_ms)
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
        "checkpw" => {
            if args.len() < 4 {
                usage();
                std::process::exit(2);
            }
            let db_file = PathBuf::from(&args[2]);
            let password = args[3].as_str();
            check_password(&db_file, password)
        }
        "encode" => {
            let (src, dst) = two_paths(&args);
            let key = parse_key(&args[4]);
            encode_file_with_key(&src, &dst, key)
        }
        "encode-random" => {
            let (src, dst) = two_paths(&args);
            jackcess::crypt::JetEncodingHandler::add_encoding(&src, &dst)
        }
        "decode" => {
            let (src, dst) = two_paths(&args);
            decode_file(&src, &dst)
        }
        "pw-set" => {
            let (src, dst) = two_paths(&args);
            PasswordUtil::set_database_password(&src, &dst, &args[4])
        }
        "pw-remove" => {
            let (src, dst) = two_paths(&args);
            PasswordUtil::remove_database_password(&src, &dst)
        }
        "encode-in-place" => {
            let file = PathBuf::from(&args[2]);
            jackcess::crypt::JetEncodingHandler::add_encoding_with_key_in_place(
                &file,
                parse_key(&args[3]),
            )
        }
        "decode-in-place" => {
            let file = PathBuf::from(&args[2]);
            jackcess::crypt::JetEncodingHandler::remove_encoding_in_place(&file)
        }
        "pw-set-in-place" => {
            let file = PathBuf::from(&args[2]);
            PasswordUtil::set_database_password_in_place(&file, &args[3])
        }
        "pw-remove-in-place" => {
            let file = PathBuf::from(&args[2]);
            PasswordUtil::remove_database_password_in_place(&file)
        }
        _ => {
            usage();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

fn two_paths(args: &[String]) -> (PathBuf, PathBuf) {
    if args.len() < 4 {
        usage();
        std::process::exit(2);
    }
    (PathBuf::from(&args[2]), PathBuf::from(&args[3]))
}

fn parse_key(value: &str) -> i32 {
    i64::from_str_radix(value.trim_start_matches("0x"), 16)
        .map(|v| v as i32)
        .unwrap_or(DEFAULT_KEY)
}

fn usage() {
    eprintln!("usage: jetcrypt gen <outdir> [--password P] [--key HEX] [--now-ms N]");
    eprintln!("       jetcrypt read <dbfile> <password|-> <out.txt>");
    eprintln!("       jetcrypt checkpw <dbfile> <password>");
    eprintln!("       jetcrypt encode <src> <dst> <keyHex>");
    eprintln!("       jetcrypt encode-random <src> <dst>");
    eprintln!("       jetcrypt decode <src> <dst>");
    eprintln!("       jetcrypt pw-set <src> <dst> <password>");
    eprintln!("       jetcrypt pw-remove <src> <dst>");
    eprintln!("       jetcrypt encode-in-place <file> <keyHex>");
    eprintln!("       jetcrypt decode-in-place <file>");
    eprintln!("       jetcrypt pw-set-in-place <file> <password>");
    eprintln!("       jetcrypt pw-remove-in-place <file>");
}

/// The whole `JetCryptGen gen` work: the plain base and every encoded /
/// password protected variant.
fn gen(out_dir: &Path, password: &str, key: i32, now_ms: Option<i64>) -> Result<()> {
    std::fs::create_dir_all(out_dir)?;

    let base = out_dir.join("base.mdb");
    let _ = std::fs::remove_file(&base);
    create_base(&base, now_ms)?;
    println!("wrote base.mdb size={}", std::fs::metadata(&base)?.len());

    // (a) jet encoding under a fixed key
    let encoded = out_dir.join("encoded.mdb");
    encode_file_with_key(&base, &encoded, key)?;
    println!(
        "wrote encoded.mdb size={}",
        std::fs::metadata(&encoded)?.len()
    );

    // encoded -> plain, through the same transform
    let decoded = out_dir.join("decoded.mdb");
    decode_file(&encoded, &decoded)?;
    println!(
        "wrote decoded.mdb size={}",
        std::fs::metadata(&decoded)?.len()
    );

    // (b) the jet database password
    let password_file = out_dir.join("password.mdb");
    PasswordUtil::set_database_password(&base, &password_file, password)?;
    println!(
        "wrote password.mdb size={}",
        std::fs::metadata(&password_file)?.len()
    );

    // the random-key variant: its bytes cannot be reproduced (the key is
    // random), but the rows it encodes are the same
    let random_encoded = out_dir.join("randomencoded.mdb");
    jackcess::crypt::JetEncodingHandler::add_encoding(&base, &random_encoded)?;
    println!(
        "wrote randomencoded.mdb size={}",
        std::fs::metadata(&random_encoded)?.len()
    );

    // password -> no password
    let nopassword = out_dir.join("nopassword.mdb");
    PasswordUtil::remove_database_password(&password_file, &nopassword)?;
    println!(
        "wrote nopassword.mdb size={}",
        std::fs::metadata(&nopassword)?.len()
    );

    // (c) password and encoding: the password is set first, because the
    // password rewrite opens the database and an encoded database needs a
    // codec provider to open (the `PasswordUtil` path forms do not pass one)
    let both = out_dir.join("both.mdb");
    encode_file_with_key(&password_file, &both, key)?;
    println!("wrote both.mdb size={}", std::fs::metadata(&both)?.len());

    // both -> password only
    let bothplain = out_dir.join("bothplain.mdb");
    decode_file(&both, &bothplain)?;
    println!(
        "wrote bothplain.mdb size={}",
        std::fs::metadata(&bothplain)?.len()
    );

    // password only -> plain
    let bothnopw = out_dir.join("bothnopw.mdb");
    PasswordUtil::remove_database_password(&bothplain, &bothnopw)?;
    println!(
        "wrote bothnopw.mdb size={}",
        std::fs::metadata(&bothnopw)?.len()
    );

    // rows read back through the codec provider, for every variant
    let mut dump = String::new();
    for name in [
        "base.mdb",
        "encoded.mdb",
        "decoded.mdb",
        "password.mdb",
        "nopassword.mdb",
        "both.mdb",
        "bothplain.mdb",
        "bothnopw.mdb",
        "randomencoded.mdb",
    ] {
        let f = out_dir.join(name);
        dump.push_str(&format!("== {}\n", name));
        dump.push_str(&dump_rows(&f, Some(password))?);
    }
    let rows = out_dir.join("rows.txt");
    std::fs::write(&rows, dump.as_bytes())?;
    println!("wrote rows.txt size={}", std::fs::metadata(&rows)?.len());

    // a wrong password does not fail to open (the jet password is not
    // authenticated by the format), so the mismatch is reported by comparing
    // the stored password
    let wrong_password = format!("wrong-{}", password);
    let stored = open_and_get_password(&password_file, &wrong_password)?;
    let mut wrong = String::new();
    wrong.push_str(&format!("stored={}\n", stored));
    wrong.push_str(&format!("matches={}\n", stored == wrong_password));
    let wrong_out = out_dir.join("wrongpassword.txt");
    std::fs::write(&wrong_out, wrong.as_bytes())?;
    println!(
        "wrote wrongpassword.txt size={}",
        std::fs::metadata(&wrong_out)?.len()
    );

    Ok(())
}

/// `JetCryptGen.createBase`: the "Test" table of `Gen.java`'s "rows" scenario.
fn create_base(base: &Path, now_ms: Option<i64>) -> Result<()> {
    let mut db = Database::create(FileFormat::V2003, base)?;
    db.now_ms = now_ms;
    let spec = TableSpec::new(
        TABLE,
        vec![
            ColumnSpec::new("ID", jackcess::datatype::DataType::Long).with_auto_number(true),
            ColumnSpec::new("Name", jackcess::datatype::DataType::Text).with_length(50),
            ColumnSpec::new("Value", jackcess::datatype::DataType::Double),
        ],
    )
    .with_index(IndexSpec::primary_key(
        "PrimaryKey",
        vec![("ID".to_string(), true)],
    ));
    db.create_table(&spec)?;
    // one row at a time, like `Table.addRowFromMap` and the `mdbgen` "rows"
    // scenario: the per-row index writes are observable in the file
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

fn open_and_get_password(db_file: &Path, password: &str) -> Result<String> {
    let mut db = open_db(db_file, Some(password))?;
    Ok(db
        .get_database_password()?
        .unwrap_or_else(|| "null".to_string()))
}

fn check_password(db_file: &Path, password: &str) -> Result<()> {
    let stored = open_and_get_password(db_file, password)?;
    println!("opened-with-wrong-password password={}", stored);
    if stored == password {
        println!("PASSWORD-ACCEPTED");
        Ok(())
    } else {
        println!("PASSWORD-REJECTED stored={}", stored);
        std::process::exit(3);
    }
}

/// `JetCryptGen.dumpRows`: the rows of the test table and the two SID tables,
/// with every value in an exact, implementation independent representation.
fn dump_rows(db_file: &Path, password: Option<&str>) -> Result<String> {
    let mut db = open_db(db_file, password)?;
    let mut out = String::new();
    let pwd = db
        .get_database_password()?
        .unwrap_or_else(|| "null".to_string());
    out.push_str(&format!("password={}\n", pwd));

    // the "Test" table: ID, Name, Value
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

/// The exact textual form of a value, byte for byte what
/// `JetCryptGen.fmt` writes.
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
        Some(Value::Decimal { unscaled, scale }) => {
            // `BigDecimal.toString`; the dump never reads a decimal column,
            // but keep the shape sensible
            format!("x:{}E-{}", unscaled, scale)
        }
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
