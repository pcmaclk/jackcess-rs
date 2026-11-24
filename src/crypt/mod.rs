//! Port of `com.healthmarketscience.jackcess.crypt` and its `impl` package
//! (jackcess-encrypt 5.0.2): the jet page encoding ("Encode/Decode
//! Database"), the jet database password, and the codec provider which reads
//! an encoded database.
//!
//! # What is supported
//!
//! * **Jet 4 page encoding** (`VERSION_4`, the `.mdb` files access 2000/2003
//!   writes): RC4 under the encoding key xored with the page number, with the
//!   key stored in the clear in page 0.  Both reading (through
//!   [`CryptCodecProvider`]) and adding/removing it ([`JetEncodingHandler`]).
//! * **The jet database password**: page 0 password field plus the SID
//!   re-masking which keeps the ACEs valid, for setting/removing it on a file
//!   ([`PasswordUtil`]) or on an open database ([`JetPasswordHandler`]).
//! * **MSISAM** (Microsoft Money `.mny`) page encoding: both the hashed
//!   password scheme (MD5/SHA-1 digest, RC4 per page keys, pages 1-14 encoded,
//!   password verifier in page 0) and the old jet style key derivation, in
//!   [`msisam`].  See [`msisam::MsisamCryptCodecHandler`].
//! * **Office encryption** (`.accdb`): the agile scheme (read and write,
//!   [`add_office_encryption`] / [`remove_office_encryption`]) and the older
//!   ECMA standard, RC4 CryptoAPI and Office binary RC4 schemes (read and
//!   removal).  An encrypted database is opened with
//!   [`Database::open_path_with_password`].
//!
//! # The shape of an encrypted `.accdb`
//!
//! An Access database is a page file, not an OLE compound file, so
//! jackcess-encrypt (and this port) do not use the `EncryptionInfo` /
//! `EncryptedPackage` streams.  Page 0 stays in the clear and carries the
//! per file *encoding key* at [`crate::format::JetFormat::offset_encoding_key`]
//! (office uses it as a salt), the password field (the low byte of that key,
//! repeated, under the creation date mask) and the `EncryptionInfo` structure
//! at [`office::CRYPT_STRUCTURE_OFFSET`].  Every page above 0 is then encrypted as
//! a whole sector: AES-CBC under the content key, with an IV derived from the
//! key data salt and the encoding key xored with the page number.  That is why
//! the "segment number" of the agile spec is the Access page number here, and
//! why the page transform moves no data.
//!
//! # Documented gaps
//!
//! * **Writing jet 3**.  The page transform is the same for a `VERSION_3`
//!   database and the read path decodes one (`tools/verify-jet3-crypt.ps1`
//!   compares it against jackcess-encrypt over the real Access 97 fixtures),
//!   but jackcess cannot write jet 3 and neither can this port.  Adding or
//!   removing the encoding and setting the password on jet 3 are refused with
//!   the Java messages ("... only supported for jet 4 databases").  Because
//!   the verification has to start from an *encoded* jet 3 file, the test
//!   tooling provides the two writes the public API refuses:
//!   [`recode_jet3`] (the `JetEncodingHandler.recode` page loop on a
//!   `VERSION_3` file) and [`set_jet3_password_file`] (the page 0 password
//!   field, without the creation date mask jet 3 does not have).  The
//!   Java-compatible entry points never call them, so they keep the Java
//!   refusals.
//! * **The jet database password** is not authenticated by the file format at
//!   all (the pages are not encrypted and the SID mask is derived from the
//!   password field), so opening a password-protected database never fails
//!   for a wrong password, in jackcess-encrypt 5.0.2 or here.  The only
//!   comparison the file supports is [`Database::verify_database_password`]
//!   (Java's `Database.getDatabasePassword`), which both verification sides
//!   use.
//! * **Writing MSISAM**: jackcess has no MSISAM file creation template, and a
//!   `.mny` is opened read-only here, so adding or removing its page encoding
//!   is not offered.  Reading (plain and password protected) is complete.
//! * **Office providers which are not written by ms access**: the agile
//!   descriptor is read with AES-CBC (and AES-ECB), the SHA family and MD5;
//!   RC2, DES/3DES, CFB/CCM/GCM chaining, MD2/MD4/RIPEMD/Whirlpool and the
//!   certificate key encryptor are refused with an "Unsupported encryption
//!   parameter" error.  Extensible encryption (version 3,3 / 4,3) is refused,
//!   as in Java.  Only agile can be written: jackcess-encrypt has no writer
//!   for the older schemes either.
//! * **The agile `dataIntegrity` HMAC** is parsed but not verified, exactly
//!   like jackcess-encrypt 5.0.2 (`AgileEncryptionProvider` never looks at
//!   it).  No `.accdb` ms access wrote carries the element; Access has no
//!   `EncryptedPackage` stream for the HMAC to cover.
//! * **The Java reference derives the header/verifier with per page key work**:
//!   this port caches the spin-count hash once per provider (it depends only on
//!   the password), which produces the same keys with less work.

pub mod codec;
pub mod encoding;
pub mod jet;
pub mod msisam;
pub mod office;
pub mod password;
pub mod rewriter;

#[cfg(all(test, feature = "fixtures"))]
mod real_file_tests;

pub use codec::{
    apply_header_mask, apply_page_number, create_encoding_key, get_password_mask, is_blank_key,
    read_header_page, read_password_region, CodecHandler, CodecProvider, DefaultCodecProvider,
    DummyHandler, KeyCache, UnsupportedHandler,
};
pub use encoding::{
    decode_file, encode_file, encode_file_with_key, recode_jet3, EncodeKey, JetEncodingHandler,
};
pub use jet::JetCryptCodecHandler;
pub use msisam::MsisamCryptCodecHandler;
pub use office::{
    AgileEncryptionConfig, AgileEncryptionWriter, AgileKeyMaterial, OfficeEncryptionHandler,
};
pub use password::{set_jet3_password_file, JetPasswordHandler, PasswordUtil, SidRemasker};
pub use rewriter::{count_pages, read_page, write_page, DbFileRewriter};

use crate::error::Result;
use crate::format::{Charset, CodecType};
use crate::pagechannel::PageChannel;
use std::cell::RefCell;
use std::path::Path;

/// Port of `com.healthmarketscience.jackcess.crypt.PasswordCallback`: supplies
/// the password when a database actually needs one.
pub trait PasswordCallback {
    fn get_password(&mut self) -> Option<String>;
}

impl<F> PasswordCallback for F
where
    F: FnMut() -> Option<String>,
{
    fn get_password(&mut self) -> Option<String> {
        self()
    }
}

/// Port of `com.healthmarketscience.jackcess.crypt.CryptCodecProvider`: a
/// [`CodecProvider`] which knows the jet page encoding, the Office encryption
/// schemes and the MSISAM (Microsoft Money) encryption.
///
/// A password can be supplied directly or through a [`PasswordCallback`]; the
/// callback is only invoked when a handler actually asks for it (which the
/// jet page encoding never does, because its key is in the file).  The
/// password is required for the MSISAM (modern encryption only) and Office
/// codecs: an encrypted `.accdb` or `.mny` refuses to open with an
/// [`crate::error::Error::InvalidCredentials`] when it is missing or wrong,
/// like Java's `InvalidCredentialsException`.
pub struct CryptCodecProvider {
    password: Option<String>,
    callback: RefCell<Option<Box<dyn PasswordCallback>>>,
}

impl CryptCodecProvider {
    pub fn new() -> CryptCodecProvider {
        CryptCodecProvider {
            password: None,
            callback: RefCell::new(None),
        }
    }

    pub fn with_password(password: &str) -> CryptCodecProvider {
        CryptCodecProvider {
            password: Some(password.to_string()),
            callback: RefCell::new(None),
        }
    }

    /// The password-less provider used to open a file whose encryption needs
    /// no password (`CryptCodecProvider()` in Java).
    pub fn from_password(password: Option<&str>) -> CryptCodecProvider {
        CryptCodecProvider {
            password: password.map(|p| p.to_string()),
            callback: RefCell::new(None),
        }
    }

    /// `CryptCodecProvider.setPassword`.
    pub fn set_password(&mut self, password: Option<String>) -> &mut Self {
        self.password = password;
        self
    }

    /// `CryptCodecProvider.getPassword`.
    pub fn password(&self) -> Option<&str> {
        self.password.as_deref()
    }

    /// `CryptCodecProvider.setPasswordCallback`: the callback is invoked when
    /// a handler needs a password, in preference to the stored one.  A plain
    /// `FnMut() -> Option<String>` implements [`PasswordCallback`], so a
    /// closure works here.
    pub fn set_password_callback<C>(&mut self, callback: C) -> &mut Self
    where
        C: PasswordCallback + 'static,
    {
        *self.callback.borrow_mut() = Some(Box::new(callback));
        self
    }

    /// The password a handler would see: the callback's value when one is set,
    /// the configured password otherwise (`CryptCodecProvider.createHandler`'s
    /// `callback` resolution).
    pub fn resolve_password(&self) -> Option<String> {
        let mut callback = self.callback.borrow_mut();
        match callback.as_mut() {
            Some(cb) => cb.get_password(),
            None => self.password.clone(),
        }
    }
}

impl Default for CryptCodecProvider {
    fn default() -> Self {
        CryptCodecProvider::new()
    }
}

impl CodecProvider for CryptCodecProvider {
    fn create_handler(
        &self,
        channel: &mut PageChannel,
        _charset: Charset,
    ) -> Result<Box<dyn CodecHandler>> {
        // determine from where to retrieve the password; the jet encoding
        // does not use it, the other codecs do
        let password = self.resolve_password();
        match channel.format.codec_type {
            CodecType::None => Ok(Box::new(DummyHandler)),
            CodecType::Jet => JetCryptCodecHandler::create(channel),
            CodecType::Msisam => {
                let charset = channel.charset;
                crate::crypt::msisam::MsisamCryptCodecHandler::create_handler(
                    channel,
                    password.as_deref(),
                    charset,
                )
            }
            CodecType::Office => {
                // an unencrypted .accdb holds a blank encoding key and needs no
                // handler at all; an encrypted one is agile / ECMA standard /
                // RC4 CryptoAPI / Office binary RC4
                crate::crypt::office::create_handler(channel, password.as_deref())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// `EncryptionUtil`'s office entry points
// ---------------------------------------------------------------------------

/// `EncryptionUtil.addOfficeAgileEncryption(srcFile, dstFile, password)`:
/// writes an agile encrypted copy of the given `.accdb` database, leaving the
/// source alone.  The default profile is
/// [`AgileEncryptionConfig::office_2010`].
pub fn add_office_encryption(src_file: &Path, dst_file: &Path, password: &str) -> Result<()> {
    add_office_encryption_with_config(
        src_file,
        dst_file,
        password,
        &AgileEncryptionConfig::office_2010(),
    )
}

/// `EncryptionUtil.addOfficeAgileEncryption(srcFile, dstFile, password,
/// config)`: writes a copy of the given `.accdb` database encrypted under the
/// given algorithms, leaving the source alone.
pub fn add_office_encryption_with_config(
    src_file: &Path,
    dst_file: &Path,
    password: &str,
    config: &AgileEncryptionConfig,
) -> Result<()> {
    DbFileRewriter::rewrite_to(src_file, dst_file, |src, dst| {
        office::OfficeEncryptionHandler::add_encryption(src, dst, password, config)
    })
}

/// `EncryptionUtil.addOfficeAgileEncryption(dbFile, password)`: encrypts the
/// given `.accdb` database, replacing the file with the result.
pub fn add_office_encryption_in_place(db_file: &Path, password: &str) -> Result<()> {
    add_office_encryption_in_place_with_config(
        db_file,
        password,
        &AgileEncryptionConfig::office_2010(),
    )
}

/// `EncryptionUtil.addOfficeAgileEncryption(dbFile, password, config)`:
/// encrypts the given `.accdb` database with the given algorithms, replacing
/// the file with the result.
pub fn add_office_encryption_in_place_with_config(
    db_file: &Path,
    password: &str,
    config: &AgileEncryptionConfig,
) -> Result<()> {
    DbFileRewriter::rewrite_in_place(db_file, |src, dst| {
        office::OfficeEncryptionHandler::add_encryption(src, dst, password, config)
    })
}

/// The deterministic form of [`add_office_encryption_with_config`]: the agile
/// key material and the page 0 encoding key are given rather than generated,
/// so the encrypted bytes are reproducible.  (ms access, and the Java library,
/// draw both from `SecureRandom`; this is for byte comparisons, not for real
/// databases.)
pub fn add_office_encryption_with_key_material(
    src_file: &Path,
    dst_file: &Path,
    password: &str,
    config: &AgileEncryptionConfig,
    material: &AgileKeyMaterial,
    encoding_key: i32,
) -> Result<()> {
    DbFileRewriter::rewrite_to(src_file, dst_file, |src, dst| {
        office::OfficeEncryptionHandler::add_encryption_with_key_material(
            src,
            dst,
            password,
            config,
            material,
            encoding_key,
        )
    })
}

/// `EncryptionUtil.removeOfficeEncryption(srcFile, dstFile, password)`:
/// writes a decrypted copy of the given `.accdb` database, leaving the source
/// alone.  Works for every scheme [`CryptCodecProvider`] can open.
pub fn remove_office_encryption(src_file: &Path, dst_file: &Path, password: &str) -> Result<()> {
    DbFileRewriter::rewrite_to(src_file, dst_file, |src, dst| {
        office::OfficeEncryptionHandler::remove_encryption(src, dst, password)
    })
}

/// `EncryptionUtil.removeOfficeEncryption(dbFile, password)`: removes the
/// encryption from the given `.accdb` database, replacing the file with the
/// result.
pub fn remove_office_encryption_in_place(db_file: &Path, password: &str) -> Result<()> {
    DbFileRewriter::rewrite_in_place(db_file, |src, dst| {
        office::OfficeEncryptionHandler::remove_encryption(src, dst, password)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::ColumnSpec;
    use crate::database::Database;
    use crate::datatype::DataType;
    use crate::format::FileFormat;
    use crate::tablecreator::{IndexSpec, TableSpec};
    use crate::value::Value;
    use crate::Error;
    use std::path::{Path, PathBuf};

    #[test]
    fn provider_resolves_the_callback_over_the_stored_password() {
        let mut provider = CryptCodecProvider::with_password("stored");
        assert_eq!(provider.resolve_password().as_deref(), Some("stored"));
        provider.set_password_callback(|| Some("callback".to_string()));
        assert_eq!(provider.resolve_password().as_deref(), Some("callback"));
        assert_eq!(provider.resolve_password().as_deref(), Some("callback"));
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

    /// The MSISAM (Microsoft Money) read path against the real
    /// jackcess-encrypt fixtures: the convenience `open_path` provides the
    /// MSISAM codec, the old encoding needs no password (and ignores a wrong
    /// one), the modern encoding verifies it, and the Java refusals (default
    /// codec provider, file creation) keep their messages.
    #[test]
    fn msisam_files_read_through_the_msisam_codec() {
        let data = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external")
            .join("jackcessencrypt-test");
        let plain = data.join("money2001.mny");
        let old_pwd = data.join("money2001-pwd.mny");
        let modern = data.join("money2008.mny");
        let modern_pwd = data.join("money2008-pwd.mny");
        if !plain.exists() {
            eprintln!("skipping MSISAM read test: {} is missing", plain.display());
            return;
        }

        // the plain (read-only) open decodes the pages and reads the rows
        let mut db = Database::open_path(&plain).unwrap();
        assert_eq!(db.format.name, "MSISAM");
        assert!(db.is_read_only(), "a MSISAM database is opened read-only");
        assert_eq!(db.charset, Charset::Utf16Le);
        let names = db.get_table_names().unwrap();
        assert_eq!(names.len(), 66);
        assert!(names.iter().any(|n| n == "CRNC"));
        let rows = db.read_rows_with_ids("CRNC").unwrap();
        let first = rows
            .iter()
            .find_map(|(_, row)| row.as_ref())
            .expect("CRNC has rows");
        assert_eq!(first[0], Value::Long(1));
        assert_eq!(first[1], Value::Text("Argentinean peso".into()));
        assert_eq!(first[2], Value::Long(11274));
        assert_eq!(first[4], Value::Text("ARS".into()));
        assert_eq!(first[5], Value::Text("/ARSUS".into()));

        // an old style password protected file opens without (and with any)
        // password: the key is derived from the page 0 password field
        let (pwd_names, pwd_rows) = {
            let mut db = Database::open_path_with_password(&old_pwd, None).unwrap();
            (
                db.get_table_names().unwrap(),
                db.read_rows_with_ids("CRNC").unwrap(),
            )
        };
        assert!(pwd_rows.iter().any(|(_, row)| row.is_some()));
        {
            let password = Some("WrongPassword");
            let mut db = Database::open_path_with_password(&old_pwd, password).unwrap();
            assert_eq!(db.get_table_names().unwrap(), pwd_names);
            assert_eq!(db.read_rows_with_ids("CRNC").unwrap(), pwd_rows);
        }

        // the modern encoding verifies the password
        let mut plain_modern = Database::open_path(&modern).unwrap();
        assert_eq!(plain_modern.get_table_names().unwrap().len(), 83);
        for password in [None, Some("WrongPassword")] {
            match Database::open_path_with_password(&modern_pwd, password) {
                Err(Error::InvalidCredentials(message)) => {
                    assert_eq!(message, "Incorrect password provided")
                }
                Err(other) => panic!("expected InvalidCredentials, got {:?}", other),
                Ok(_) => panic!("expected InvalidCredentials, got a successful open"),
            }
        }
        let mut db = Database::open_path_with_password(&modern_pwd, Some("Test12345")).unwrap();
        let rows = db.read_rows_with_ids("CRNC").unwrap();
        let first = rows.iter().find_map(|(_, row)| row.as_ref()).unwrap();
        assert_eq!(first[0], Value::Long(1));
        assert_eq!(first[1], Value::Text("Argentine peso".into()));

        // the Java `DefaultCodecProvider` refuses a MSISAM database (it is
        // always encoded) when it is supplied explicitly
        match Database::open_path_with_provider(&plain, &DefaultCodecProvider::INSTANCE) {
            Err(Error::Jackcess(message)) => assert_eq!(message, codec::DECODE_NOT_SUPPORTED),
            Err(other) => panic!("expected the default provider refusal, got {:?}", other),
            Ok(_) => panic!("expected the default provider refusal, got a successful open"),
        }

        // file creation is refused with the Java message
        let probe = test_tmp_dir().join("msisam_create_probe.mny");
        let _ = std::fs::remove_file(&probe);
        match Database::create(FileFormat::Msisam, &probe) {
            Err(Error::Jackcess(message)) => assert_eq!(
                message,
                format!(
                    "File format MSISAM [MSISAM] does not support file creation for {}",
                    probe.display()
                )
            ),
            Err(other) => panic!("expected the create refusal, got {:?}", other),
            Ok(_) => panic!("expected the create refusal, got a successful create"),
        }
        assert!(!probe.exists(), "the refused creation wrote a file");
    }

    /// The real Access 97 fixtures `tools/verify-jet3.ps1` uses.  A test whose
    /// fixture is missing is skipped, so the suite also runs without
    /// `external/`.
    fn jet3_fixture(name: &str) -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external")
            .join("jackcess-test-resources")
            .join("data")
            .join("V1997")
            .join(name);
        if path.exists() {
            Some(path)
        } else {
            eprintln!("skipping jet 3 crypt test: {} is missing", path.display());
            None
        }
    }

    /// The whole jet 3 read path: encode a real Access 97 database with the
    /// test side encoder (the Java handler and this port's public API refuse
    /// jet 3 writes), open it through the codec provider, compare the reads
    /// with the plain database, and decode it back to the original bytes.
    #[test]
    fn jet3_encoding_reads_through_the_codec_provider_and_decodes_back() {
        let Some(fixture) = jet3_fixture("common1V1997.mdb") else {
            return;
        };
        let dir = test_tmp_dir();
        let encoded = dir.join("crypt_jet3_encoded.mdb");
        let decoded = dir.join("crypt_jet3_decoded.mdb");
        remove_all(&[&encoded, &decoded]);
        let original = std::fs::read(&fixture).unwrap();
        let page_size = 2048usize;

        recode_jet3(&fixture, &encoded, true, 0x1234_abcd).unwrap();
        let encoded_bytes = std::fs::read(&encoded).unwrap();
        assert_eq!(encoded_bytes.len(), original.len());
        // page 0 only changes at the encoding key (which sits in the masked
        // header, so its stored bytes are the key xor the mask)
        assert_eq!(
            &encoded_bytes[..62],
            &original[..62],
            "page 0 changed before the encoding key"
        );
        assert_eq!(
            &encoded_bytes[66..page_size],
            &original[66..page_size],
            "page 0 changed after the encoding key"
        );
        assert_ne!(
            &encoded_bytes[62..66],
            &original[62..66],
            "the encoding key was not written"
        );
        // every page above 0 is transformed
        assert_ne!(
            &encoded_bytes[page_size..page_size + 64],
            &original[page_size..page_size + 64]
        );

        // the plain database reads the same rows as the encoded one
        let (plain_names, plain_rows) = {
            let mut db = Database::open_path(&fixture).unwrap();
            (
                db.get_table_names().unwrap(),
                db.read_rows_with_ids("Table1").unwrap(),
            )
        };
        let mut coded =
            Database::open_path_with_provider(&encoded, &CryptCodecProvider::new()).unwrap();
        assert_eq!(coded.format.name, "VERSION_3");
        assert_eq!(coded.get_table_names().unwrap(), plain_names);
        assert_eq!(coded.read_rows_with_ids("Table1").unwrap(), plain_rows);
        // the password field is in page 0, which is never encoded
        assert_eq!(coded.get_database_password().unwrap(), None);

        // the default provider refuses an encoded jet 3 database, like the
        // Java `DefaultCodecProvider`
        assert!(Database::open_path(&encoded).is_err());

        recode_jet3(&encoded, &decoded, false, 0).unwrap();
        assert_eq!(std::fs::read(&decoded).unwrap(), original);
        remove_all(&[&encoded, &decoded]);
    }

    /// The jet 3 database password: the page 0 field is stored without the
    /// creation date mask jet 3 does not have, `get_database_password` reads
    /// it, and supplying a wrong (or no) password never fails to open - the
    /// jet password is not authenticated by the file format.
    #[test]
    fn jet3_database_password_field_reads_like_the_java_reference() {
        let Some(fixture) = jet3_fixture("common1V1997.mdb") else {
            return;
        };
        let dir = test_tmp_dir();
        let password_file = dir.join("crypt_jet3_password.mdb");
        let removed = dir.join("crypt_jet3_password_removed.mdb");
        let encoded = dir.join("crypt_jet3_password_encoded.mdb");
        remove_all(&[&password_file, &removed, &encoded]);
        let original = std::fs::read(&fixture).unwrap();
        let plain_rows = {
            let mut db = Database::open_path(&fixture).unwrap();
            db.read_rows_with_ids("Table1").unwrap()
        };

        set_jet3_password_file(&fixture, &password_file, Some("s3cret-Pa55")).unwrap();
        {
            // a wrong password still opens; the stored field is what reports it
            let mut db =
                Database::open_path_with_password(&password_file, Some("wrong-password")).unwrap();
            assert_eq!(
                db.get_database_password().unwrap().as_deref(),
                Some("s3cret-Pa55")
            );
            assert!(db.verify_database_password("s3cret-Pa55").unwrap());
            assert!(!db.verify_database_password("wrong").unwrap());
            assert_eq!(db.read_rows_with_ids("Table1").unwrap(), plain_rows);
        }
        {
            // the original file has no password
            let mut db = Database::open_path_with_password(&fixture, None).unwrap();
            assert_eq!(db.get_database_password().unwrap(), None);
        }

        // removing the field restores the original bytes
        set_jet3_password_file(&password_file, &removed, None).unwrap();
        assert_eq!(std::fs::read(&removed).unwrap(), original);

        // the password field is in page 0, so it survives the encoding
        recode_jet3(&password_file, &encoded, true, 0x0bad_f00d).unwrap();
        let mut db = Database::open_path_with_password(&encoded, None).unwrap();
        assert_eq!(
            db.get_database_password().unwrap().as_deref(),
            Some("s3cret-Pa55")
        );
        assert_eq!(db.read_rows_with_ids("Table1").unwrap(), plain_rows);
        remove_all(&[&password_file, &removed, &encoded]);
    }

    /// The whole feature end to end, without a Java reference: a database with
    /// a password and the page encoding is read back through the codec
    /// provider, then both transforms are removed again and the file must be
    /// the original bytes.
    #[test]
    fn encode_and_password_round_trip() {
        let dir = test_tmp_dir();
        let base = dir.join("crypt_rt_base.mdb");
        let password_copy = dir.join("crypt_rt_password.mdb");
        let both = dir.join("crypt_rt_both.mdb");
        let bothplain = dir.join("crypt_rt_bothplain.mdb");
        let nopassword = dir.join("crypt_rt_nopassword.mdb");
        remove_all(&[&base, &password_copy, &both, &bothplain, &nopassword]);

        {
            let mut db = Database::create(FileFormat::V2003, &base).unwrap();
            let spec = TableSpec::new(
                "T",
                vec![
                    ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                    ColumnSpec::new("Name", DataType::Text).with_length(20),
                ],
            )
            .with_index(IndexSpec::primary_key(
                "PrimaryKey",
                vec![("ID".to_string(), true)],
            ));
            db.create_table(&spec).unwrap();
            for i in 0..3 {
                db.add_rows("T", &[vec![Value::Null, Value::Text(format!("n{}", i))]])
                    .unwrap();
            }
            db.flush().unwrap();
        }
        let base_bytes = std::fs::read(&base).unwrap();

        // the password, then the encoding on top of it
        PasswordUtil::set_database_password(&base, &both, "pw-Test1").unwrap();
        JetEncodingHandler::add_encoding_with_key_in_place(&both, 0x0BAD_F00D).unwrap();

        // the encoded + password protected file opens through the provider and
        // reads the rows and the stored password
        {
            let mut db = Database::open_path_with_password(&both, Some("pw-Test1")).unwrap();
            assert_eq!(
                db.get_database_password().unwrap().as_deref(),
                Some("pw-Test1")
            );
            assert!(db.verify_database_password("pw-Test1").unwrap());
            assert!(!db.verify_database_password("wrong").unwrap());
            let rows = db.read_rows_with_ids("T").unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(
                rows[0].1.as_ref().unwrap()[1],
                Value::Text("n0".to_string())
            );
        }

        // the default provider refuses the encoded file, like the Java
        // `DefaultCodecProvider`
        assert!(Database::open_path(&both).is_err());

        // remove the encoding -> the password only file, which is the same as
        // setting the password on the base directly
        decode_file(&both, &bothplain).unwrap();
        PasswordUtil::set_database_password(&base, &password_copy, "pw-Test1").unwrap();
        assert_eq!(
            std::fs::read(&bothplain).unwrap(),
            std::fs::read(&password_copy).unwrap()
        );

        // remove the password -> the original bytes
        PasswordUtil::remove_database_password(&bothplain, &nopassword).unwrap();
        assert_eq!(std::fs::read(&nopassword).unwrap(), base_bytes);

        remove_all(&[&base, &password_copy, &both, &bothplain, &nopassword]);
    }
}
