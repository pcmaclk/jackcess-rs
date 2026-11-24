//! Unit tests pinned to the **real** Access / jackcess-encrypt files in
//! `external/jackcessencrypt-test`, which `tools/verify-realcrypt.ps1` also
//! verifies against the Java reference.
//!
//! Every test is skipped when the fixture is missing, so the suite still runs
//! without `external/`.  The pinned values (encoding keys, password fields,
//! table contents and the SHA-256 of the decrypted Office files) were read
//! from the Java library; the harness compares the two implementations, and
//! these tests keep a change from silently shifting the Rust side alone.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::apply_header_mask;
use crate::crypt::office::CRYPT_STRUCTURE_OFFSET;
use crate::crypt::rewriter::read_file_format;
use crate::crypt::{
    decode_file, encode_file_with_key, recode_jet3, remove_office_encryption, CryptCodecProvider,
    PasswordUtil,
};
use crate::database::Database;
use crate::format::JetFormat;
use crate::value::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> Option<PathBuf> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("external")
        .join("jackcessencrypt-test")
        .join(name);
    if path.exists() {
        Some(path)
    } else {
        eprintln!("skipping real crypt test: {} is missing", path.display());
        None
    }
}

fn test_tmp_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn remove_all(paths: &[&PathBuf]) {
    for p in paths {
        let _ = std::fs::remove_file(p);
    }
}

/// The demasked page 0 of the given file and its format.
fn header_page(path: &Path) -> (JetFormat, ByteBuf) {
    let mut file = std::fs::File::open(path).unwrap();
    let format = read_file_format(&mut file).unwrap();
    let mut page = vec![0u8; format.page_size];
    crate::crypt::read_page(&mut file, &mut page, 0, format.page_size).unwrap();
    let mut header = ByteBuf::wrap_le(page);
    apply_header_mask(&mut header, &format).unwrap();
    (format, header)
}

/// The first two rows of the `Table1` the jackcess-encrypt test files hold.
fn check_jet_table1(db: &mut Database) {
    let rows = db.read_rows_with_ids("Table1").unwrap();
    let rows: Vec<&Vec<Value>> = rows.iter().filter_map(|(_, r)| r.as_ref()).collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Long(1));
    assert_eq!(rows[0][1], Value::Text("hello".into()));
    assert_eq!(rows[0][2], Value::Long(0));
    assert_eq!(rows[1][0], Value::Long(2));
    assert_eq!(rows[1][1], Value::Text("world".into()));
    assert_eq!(rows[1][2], Value::Long(42));
}

/// Every masked SID of the two SID columns, sorted, as `("{table}:{hex}")`.
fn read_sids(db_file: &Path) -> Vec<String> {
    let mut db = Database::open_path_read_only(db_file).unwrap();
    let mut sids = Vec::new();
    for (table_name, col_name) in [("MSysACEs", "SID"), ("MSysObjects", "Owner")] {
        let Some((page, flags)) = db.table_lookup.get(&table_name.to_uppercase()).copied() else {
            continue;
        };
        db.load_table(page, table_name, flags).unwrap();
        let ci = {
            let table = db.tables.get(&page).unwrap();
            table
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(col_name))
        };
        let Some(ci) = ci else { continue };
        let name = db.tables.get(&page).unwrap().name.clone();
        for (_, values) in db.read_rows_with_ids(&name).unwrap() {
            let Some(values) = values else { continue };
            if let Some(Value::Bytes(sid)) = values.get(ci) {
                if sid.is_empty() {
                    continue;
                }
                let hex: String = sid.iter().map(|b| format!("{:02x}", b)).collect();
                sids.push(format!("{}:{}", table_name, hex));
            }
        }
    }
    sids.sort();
    sids
}

/// The raw page 0 password field of a jet 4 database.
fn raw_password_field(db_file: &Path) -> Vec<u8> {
    let mut file = std::fs::File::open(db_file).unwrap();
    let format = read_file_format(&mut file).unwrap();
    // the field carries its own creation date mask, so the stored bytes are
    // what has to match the ms access file
    let raw = std::fs::read(db_file).unwrap();
    raw[format.offset_password..format.offset_password + format.size_password].to_vec()
}

fn check_page0_except_password(src: &Path, dst: &Path) {
    let a = std::fs::read(src).unwrap();
    let b = std::fs::read(dst).unwrap();
    assert_eq!(a.len(), b.len(), "size changed");
    let format = JetFormat::version_4();
    for i in 0..format.page_size {
        if (format.offset_password..format.offset_password + format.size_password).contains(&i) {
            continue;
        }
        assert_eq!(a[i], b[i], "page 0 byte {:#x} changed", i);
    }
}

/// `db-enc.mdb`, a real Access 2000 file encoded with "Encode/Decode
/// Database": the provider reads the same rows as the plain database and the
/// recovered key re-encodes it to the original bytes.
#[test]
fn real_jet4_encoded_file_reads_decodes_and_reencodes() {
    let Some(encoded) = fixture("db-enc.mdb") else {
        return;
    };
    let dir = test_tmp_dir();
    let plain = dir.join("real_db_enc_plain.mdb");
    let reenc = dir.join("real_db_enc_reenc.mdb");
    remove_all(&[&plain, &reenc]);

    let (format, header) = header_page(&encoded);
    assert_eq!(format.name, "VERSION_4");
    let key = header.get_i32_at(format.offset_encoding_key).unwrap();
    // the key ms access wrote is in the file (the scheme is obfuscation)
    assert_eq!(key, 0xa7b7_73e0_u32 as i32);

    {
        let mut db =
            Database::open_path_read_only_with_provider(&encoded, &CryptCodecProvider::new())
                .unwrap();
        assert_eq!(db.format.name, "VERSION_4");
        check_jet_table1(&mut db);
        assert_eq!(db.get_database_password().unwrap(), None);
    }
    // the default provider refuses the encoded file, like the Java one
    assert!(Database::open_path_read_only(&encoded).is_err());

    decode_file(&encoded, &plain).unwrap();
    let (_, plain_header) = header_page(&plain);
    assert_eq!(
        plain_header
            .get_i32_at(JetFormat::version_4().offset_encoding_key)
            .unwrap(),
        0
    );
    {
        let mut db = Database::open_path_read_only(&plain).unwrap();
        check_jet_table1(&mut db);
    }

    encode_file_with_key(&plain, &reenc, key).unwrap();
    assert_eq!(
        std::fs::read(&reenc).unwrap(),
        std::fs::read(&encoded).unwrap(),
        "re-encoding under the recovered key did not reproduce the real file"
    );
    remove_all(&[&plain, &reenc]);
}

/// `db97-enc.mdb`, a real Access 97 file encoded by jackcess-encrypt: the
/// provider reads it through the jet 3 page layout, and `recode_jet3` decodes
/// it to a plain Access 97 file and re-encodes it to the original bytes.
#[test]
fn real_jet3_encoded_file_reads_decodes_and_reencodes() {
    let Some(encoded) = fixture("db97-enc.mdb") else {
        return;
    };
    let dir = test_tmp_dir();
    let plain = dir.join("real_db97_enc_plain.mdb");
    let reenc = dir.join("real_db97_enc_reenc.mdb");
    remove_all(&[&plain, &reenc]);

    let (format, header) = header_page(&encoded);
    assert_eq!(format.name, "VERSION_3");
    assert_eq!(format.page_size, 2048);
    let key = header.get_i32_at(format.offset_encoding_key).unwrap();
    assert_eq!(key, 0xa7e0_c0fe_u32 as i32);

    {
        let mut db =
            Database::open_path_read_only_with_provider(&encoded, &CryptCodecProvider::new())
                .unwrap();
        assert_eq!(db.format.name, "VERSION_3");
        check_jet_table1(&mut db);
    }
    assert!(Database::open_path_read_only(&encoded).is_err());

    recode_jet3(&encoded, &plain, false, 0).unwrap();
    let (_, plain_header) = header_page(&plain);
    assert_eq!(
        plain_header
            .get_i32_at(JetFormat::version_3().offset_encoding_key)
            .unwrap(),
        0
    );
    {
        let mut db = Database::open_path_read_only(&plain).unwrap();
        assert_eq!(db.format.name, "VERSION_3");
        check_jet_table1(&mut db);
    }

    recode_jet3(&plain, &reenc, true, key).unwrap();
    assert_eq!(
        std::fs::read(&reenc).unwrap(),
        std::fs::read(&encoded).unwrap(),
        "re-encoding under the recovered key did not reproduce the real file"
    );
    remove_all(&[&plain, &reenc]);
}

/// The real database password files: the stored password reads back, and
/// adding/removing it produces the page 0 password field and the SID masks ms
/// access wrote (`pwd-set.mdb` / `pwd-removed.mdb`).
#[test]
fn real_jet_database_password_matches_ms_access() {
    let Some(pwd_set) = fixture("pwd-set.mdb") else {
        return;
    };
    let Some(pwd_removed) = fixture("pwd-removed.mdb") else {
        return;
    };
    let Some(pwd_none) = fixture("pwd-none.mdb") else {
        return;
    };
    let dir = test_tmp_dir();
    let removed = dir.join("real_pwd_set_removed.mdb");
    let added = dir.join("real_pwd_none_set.mdb");
    remove_all(&[&removed, &added]);

    {
        let mut db = Database::open_path_with_password(&pwd_set, Some("testpassword")).unwrap();
        assert_eq!(
            db.get_database_password().unwrap().as_deref(),
            Some("testpassword")
        );
        assert!(db.verify_database_password("testpassword").unwrap());
        assert!(!db.verify_database_password("password").unwrap());
        // the password files are a different database (one row, not the two
        // rows of the encoded fixtures)
        let rows = db.read_rows_with_ids("Table1").unwrap();
        let rows: Vec<&Vec<Value>> = rows.iter().filter_map(|(_, r)| r.as_ref()).collect();
        assert_eq!(
            rows,
            vec![&vec![Value::Long(1), Value::Text("data".into())]]
        );
    }

    // removing the password from the ms access file gives the field and the
    // SIDs the ms access produced password-less file holds
    PasswordUtil::remove_database_password(&pwd_set, &removed).unwrap();
    assert_eq!(
        raw_password_field(&removed),
        raw_password_field(&pwd_removed),
        "the cleared password field does not match pwd-removed.mdb"
    );
    check_page0_except_password(&pwd_set, &removed);
    assert_eq!(
        read_sids(&removed),
        read_sids(&pwd_removed),
        "the re-masked SIDs do not match pwd-removed.mdb"
    );
    {
        let mut db = Database::open_path_read_only(&removed).unwrap();
        assert_eq!(db.get_database_password().unwrap(), None);
    }

    // adding the password to the password-less file gives the field and the
    // SIDs the ms access produced password-protected file holds
    PasswordUtil::set_database_password(&pwd_none, &added, "testpassword").unwrap();
    assert_eq!(
        raw_password_field(&added),
        raw_password_field(&pwd_set),
        "the written password field does not match pwd-set.mdb"
    );
    check_page0_except_password(&pwd_none, &added);
    assert_eq!(
        read_sids(&added),
        read_sids(&pwd_set),
        "the masked SIDs do not match pwd-set.mdb"
    );
    {
        let mut db = Database::open_path_with_password(&added, Some("testpassword")).unwrap();
        assert_eq!(
            db.get_database_password().unwrap().as_deref(),
            Some("testpassword")
        );
    }
    remove_all(&[&removed, &added]);
}

/// The four real Office-encrypted `.accdb` files (agile, ECMA standard with
/// the RC4/CryptoAPI or AES flag, and the non-standard zero-iteration scheme):
/// each opens with its password, the wrong password is refused with the Java
/// message, and removing the encryption reproduces exactly the plaintext the
/// Java `EncryptionUtil.removeOfficeEncryption` writes (pinned by SHA-256).
#[test]
fn real_office_files_open_with_their_passwords_and_decrypt() {
    // (file, password, decrypted size, sha-256 of the Java-decrypted bytes)
    let cases: [(&str, &str, usize, &str); 4] = [
        (
            "db2007-enc.accdb",
            "Test123",
            430080,
            "f34967df448e99e4e87e0e7a185f2276c5fea22c2e1aa7f2fcd2597ed129171b",
        ),
        (
            "db2007-oldenc.accdb",
            "Test123",
            434176,
            "635708fd4649d78a63d88e9561af4ba1f1919733d924637c4ef472f4e4799c85",
        ),
        (
            "db2013-enc.accdb",
            "1234",
            389120,
            "7229168bf57ff52f2d1e46045887aae89daabf6ce5d3825bec521edaf63120f8",
        ),
        (
            "db-nonstandard.accdb",
            "password",
            442368,
            "49c1a9f472a55769eab26ae728de0ed89b2fbe91b3dec6d8a46a812c6967af67",
        ),
    ];

    let dir = test_tmp_dir();
    for (name, password, size, sha) in cases {
        let Some(encrypted) = fixture(name) else {
            continue;
        };
        let plain = dir.join(format!("real_{}_plain.accdb", name.replace('.', "_")));
        remove_all(&[&plain]);

        // the right password opens, the wrong one (and none) is refused with
        // the Java `InvalidCredentialsException` message
        {
            let mut db = Database::open_path_read_only_with_provider(
                &encrypted,
                &CryptCodecProvider::with_password(password),
            )
            .unwrap();
            match name {
                "db2013-enc.accdb" => {
                    let rows = db.read_rows_with_ids("Customers").unwrap();
                    let rows: Vec<&Vec<Value>> =
                        rows.iter().filter_map(|(_, r)| r.as_ref()).collect();
                    assert_eq!(rows.len(), 7);
                    assert_eq!(rows[0], &vec![Value::Long(1), Value::Text("Test".into())]);
                    assert_eq!(rows[3], &vec![Value::Long(4), Value::Null]);
                    assert_eq!(rows[6], &vec![Value::Long(7), Value::Text("f".into())]);
                }
                "db-nonstandard.accdb" => {
                    let rows = db.read_rows_with_ids("Table_One").unwrap();
                    let rows: Vec<&Vec<Value>> =
                        rows.iter().filter_map(|(_, r)| r.as_ref()).collect();
                    assert_eq!(
                        rows,
                        vec![&vec![Value::Long(1), Value::Text("test".into())]]
                    );
                }
                _ => {
                    let rows = db.read_rows_with_ids("Table1").unwrap();
                    let rows: Vec<&Vec<Value>> =
                        rows.iter().filter_map(|(_, r)| r.as_ref()).collect();
                    assert_eq!(rows, vec![&vec![Value::Long(1), Value::Text("foo".into())]]);
                }
            }
        }
        for wrong in [Some("WrongPassword"), None] {
            let mut provider = CryptCodecProvider::new();
            provider.set_password(wrong.map(|p| p.to_string()));
            let err = match Database::open_path_read_only_with_provider(&encrypted, &provider) {
                Ok(_) => panic!("{} opened with {:?}", name, wrong),
                Err(err) => err,
            };
            assert_eq!(
                format!("{}", err),
                "Incorrect password provided",
                "{} with {:?}",
                name,
                wrong
            );
        }

        remove_office_encryption(&encrypted, &plain, password).unwrap();
        let bytes = std::fs::read(&plain).unwrap();
        assert_eq!(bytes.len(), size, "{} decrypted size", name);
        let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{:02x}", b))
            .collect();
        assert_eq!(digest, sha, "{} decrypted bytes", name);

        // the decrypted file needs no provider, and its page 0 carries no
        // encryption state any more
        let (format, header) = header_page(&plain);
        assert_eq!(header.get_i32_at(format.offset_encoding_key).unwrap(), 0);
        assert_eq!(header.get_u16_at(CRYPT_STRUCTURE_OFFSET).unwrap(), 0);
        let mut db = Database::open_path_read_only(&plain).unwrap();
        assert!(!db.get_table_names().unwrap().is_empty());

        remove_all(&[&plain]);
    }
}
