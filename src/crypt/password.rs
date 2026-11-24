//! Port of `com.healthmarketscience.jackcess.crypt.impl.JetPasswordHandler`,
//! `impl.SidRemasker` and `com.healthmarketscience.jackcess.crypt.PasswordUtil`.
//!
//! The jet database password is not page encryption: the pages stay readable
//! and the password only makes ms access ask for it.  Two pieces of state have
//! to agree.  The password field in page 0 is what makes ms access prompt, and
//! every SID stored in the database is masked with an rc4 keystream whose key
//! is folded out of page 0, the password field included.  Writing only the
//! password field produces a database which ms access prompts for and then
//! refuses to open, reporting that the user does not have permission.
//!
//! So changing the password means re-masking every SID under the new key,
//! which [`SidRemasker`] does.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::{
    apply_header_mask, get_password_mask, read_header_page, read_password_region,
};
use crate::crypt::rewriter::{read_file_format, read_page, write_page, DbFileRewriter};
use crate::database::Database;
use crate::error::{Error, Result};
use crate::format::JetFormat;
use crate::value::Value;
use rc4::{KeyInit, Rc4, StreamCipher};
use std::path::Path;

/// Port of `com.healthmarketscience.jackcess.crypt.impl.JetPasswordHandler`.
pub struct JetPasswordHandler;

impl JetPasswordHandler {
    /// `JetPasswordHandler.setPassword`: sets the database password, or
    /// removes it if the given password is `None` or empty.
    pub fn set_password(db: &mut Database, password: Option<&str>) -> Result<()> {
        Self::validate_password_change(db)?;

        // encode the password up front, so an unusable one is rejected before
        // anything is read or written
        let new_field = Self::create_password_field(db, password)?;

        // make sure nothing is still pending behind the changes below
        db.flush()?;

        let format = db.format.clone();
        let mut header_page = read_header_page(&mut db.channel)?;

        let old_field = header_page.get_bytes_at(format.offset_password, format.size_password)?;
        let old_key = SidRemasker::create_sid_key(&header_page, &format)?;

        Self::write_password_field(&mut header_page, &format, &new_field)?;

        if old_field == header_page.get_bytes_at(format.offset_password, format.size_password)? {
            // the database already has this password, nothing to do
            return Ok(());
        }

        let new_key = SidRemasker::create_sid_key(&header_page, &format)?;

        db.channel.start_write();
        let result = (|| -> Result<()> {
            // the SIDs are rewritten before page 0 so that a failure part way
            // through leaves the password field describing the key the
            // database started with
            SidRemasker::remask_sids(db, old_key, new_key)?;
            db.channel.write_page(&header_page, 0)
        })();
        let finished = db.channel.finish_write();
        result?;
        finished?;

        db.flush()?;
        Ok(())
    }

    fn validate_password_change(db: &Database) -> Result<()> {
        if db.is_read_only() {
            return Err(Error::IllegalState(
                "Database is read-only, cannot modify the database password".into(),
            ));
        }
        Self::validate_format(&db.format)
    }

    /// Fails unless the given format holds the password field the way this
    /// writes it.
    pub fn validate_format(format: &JetFormat) -> Result<()> {
        if format.name != "VERSION_4" {
            return Err(Error::Unsupported(format!(
                "Modifying the database password is only supported for jet 4 \
                 databases, not {}",
                format.name
            )));
        }
        Ok(())
    }

    /// `JetPasswordHandler.createPasswordField`: the contents of the password
    /// field for the given password, before the creation date mask is applied.
    /// A `None` or empty password gives the empty field which indicates no
    /// password.
    pub fn create_password_field(db: &Database, password: Option<&str>) -> Result<Vec<u8>> {
        Self::encode_password_field(&db.format, db.charset, password)
    }

    fn encode_password_field(
        format: &JetFormat,
        charset: crate::format::Charset,
        password: Option<&str>,
    ) -> Result<Vec<u8>> {
        let mut field = vec![0u8; format.size_password];
        if let Some(password) = password {
            let encoded = charset.encode(password);
            let len = encoded.len();
            if len > field.len() {
                return Err(Error::IllegalArgument(format!(
                    "Password encodes to {} bytes, which is longer than the \
                     maximum of {}",
                    len,
                    field.len()
                )));
            }
            field[..len].copy_from_slice(&encoded);
        }
        Ok(field)
    }

    /// Writes the given password field into the given header page, masked the
    /// way the database expects it (the field carries an additional mask
    /// generated from the database creation date).
    fn write_password_field(
        header_page: &mut ByteBuf,
        format: &JetFormat,
        field: &[u8],
    ) -> Result<()> {
        let mut field = field.to_vec();
        if let Some(mask) = get_password_mask(header_page, format)? {
            for (i, b) in field.iter_mut().enumerate() {
                *b ^= mask[i % mask.len()];
            }
        }
        header_page.put_bytes_at(format.offset_password, &field)
    }
}

impl Database {
    /// `Database.getDatabasePassword`: the database password the page 0 field
    /// holds, or `None` when the database has no password.
    ///
    /// Like the Java library this needs no codec provider: the field is in
    /// page 0, which is never encoded.
    ///
    /// For an Office-encrypted `.accdb` this is *not* a password: office fills
    /// the field with the low byte of the page 0 encoding key repeated, so the
    /// value is arbitrary-looking and differs per file.  Java reports it the
    /// same way (it is the value ms access would prompt for, and the Office
    /// codec is what actually checks the password), so this is kept for parity
    /// rather than special cased.
    pub fn get_database_password(&mut self) -> Result<Option<String>> {
        let format = self.format.clone();
        let mut buffer = self.channel.create_page_buffer();
        self.channel.read_root_page(&mut buffer)?;

        let mut pwd_bytes = buffer.get_bytes_at(format.offset_password, format.size_password)?;

        // de-mask password using extra password mask if necessary (the extra
        // password mask is generated from the database creation date stored in
        // the header)
        if let Some(mask) = get_password_mask(&buffer, &format)? {
            for i in 0..pwd_bytes.len() {
                pwd_bytes[i] ^= mask[i % mask.len()];
            }
        }

        if pwd_bytes.iter().all(|&b| b == 0) {
            return Ok(None);
        }

        let units = self.charset.decode(&pwd_bytes);
        // remove any trailing null chars
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        Ok(Some(String::from_utf16_lossy(&units[..end])))
    }

    /// `true` when the database password the page 0 field holds is the given
    /// one.
    ///
    /// The jet "database password" is not authenticated by the format - the
    /// pages are not encrypted and jackcess (and this port) open a
    /// password-protected database without being given the password - so this
    /// comparison is the only check the file supports.
    pub fn verify_database_password(&mut self, password: &str) -> Result<bool> {
        Ok(self.get_database_password()?.as_deref() == Some(password))
    }
}

/// Port of `com.healthmarketscience.jackcess.crypt.impl.SidRemasker`: masks
/// and re-masks the SIDs a database stores.
pub struct SidRemasker;

impl SidRemasker {
    /// The SID columns which have to be re-masked, table name to column name.
    pub const SID_COLUMNS: [(&'static str, &'static str); 2] =
        [("MSysACEs", "SID"), ("MSysObjects", "Owner")];

    /// the fold which builds the SID key wraps its shift at this bit
    pub const SID_KEY_SHIFT_MODULUS: usize = 24;

    /// the SID key is used as this many little-endian bytes of rc4 key
    pub const SID_KEY_LENGTH: usize = 4;

    /// `SidRemasker.createSidKey`: folds the 32-bit key which masks every SID
    /// in the database out of the given header page.
    pub fn create_sid_key(header_page: &ByteBuf, format: &JetFormat) -> Result<i32> {
        let region = read_password_region(header_page, format)?;

        let mut key = header_page.get_i32_at(format.offset_header_date as usize)?;

        for i in 0..format.size_password {
            key ^= ((region[i * 2] as i32) & 0xFF) << (i % Self::SID_KEY_SHIFT_MODULUS);
        }

        Ok(key)
    }

    /// `SidRemasker.remaskSids`: converts every SID in the given database from
    /// the old key to the new one.
    ///
    /// Only the SID column is written.  Writing the whole row would write
    /// every column, and `MSysObjects` carries seven long value columns whose
    /// contents would land on freshly allocated pages, growing the database
    /// and orphaning what they replace.
    pub fn remask_sids(db: &mut Database, old_key: i32, new_key: i32) -> Result<()> {
        if old_key == new_key {
            return Ok(());
        }

        let mut pad = RemaskPad::new(old_key, new_key);

        for (table_name, sid_col) in SidRemasker::SID_COLUMNS {
            if !db.table_lookup.contains_key(&table_name.to_uppercase()) {
                continue;
            }

            // `findColumn`: matched case insensitively against the table's
            // columns
            let found = {
                let table = db.get_table(table_name)?;
                table
                    .columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(sid_col))
                    .map(|ci| (table.columns[ci].name.clone(), ci))
            };
            let Some((col_name, col_index)) = found else {
                continue;
            };

            // read the rows before modifying any of them
            let rows = db.read_rows_with_ids(table_name)?;

            for (row_id, values) in rows {
                let Some(values) = values else {
                    continue;
                };
                let sid = match values.get(col_index) {
                    Some(Value::Bytes(sid)) => sid.clone(),
                    _ => continue,
                };
                if sid.is_empty() {
                    continue;
                }
                let mut sid = sid;
                pad.apply(&mut sid);

                db.update_value(table_name, row_id, &col_name, Value::Bytes(sid))?;
            }
        }

        Ok(())
    }
}

/// The xor pad which converts a SID masked with the old key into the same SID
/// masked with the new one (`SidRemasker.RemaskPad`).
struct RemaskPad {
    old_key: i32,
    new_key: i32,
    pad: Vec<u8>,
}

impl RemaskPad {
    fn new(old_key: i32, new_key: i32) -> RemaskPad {
        RemaskPad {
            old_key,
            new_key,
            pad: Vec::new(),
        }
    }

    fn apply(&mut self, sid: &mut [u8]) {
        self.ensure_length(sid.len());
        for (i, b) in sid.iter_mut().enumerate() {
            *b ^= self.pad[i];
        }
    }

    fn ensure_length(&mut self, len: usize) {
        if self.pad.len() >= len {
            return;
        }

        let mut pad = create_keystream(self.old_key, len);
        let new_keystream = create_keystream(self.new_key, len);
        for i in 0..len {
            pad[i] ^= new_keystream[i];
        }
        self.pad = pad;
    }
}

/// `SidRemasker.createKeystream`: the first `len` bytes of the rc4 keystream
/// for the given key.
fn create_keystream(key: i32, len: usize) -> Vec<u8> {
    let key_bytes = (key as u32).to_le_bytes();
    debug_assert_eq!(key_bytes.len(), SidRemasker::SID_KEY_LENGTH);
    let mut cipher = Rc4::new_from_slice(&key_bytes).expect("4 byte keys are supported");
    // running the cipher over zeroes yields the keystream itself
    let mut keystream = vec![0u8; len];
    cipher.apply_keystream(&mut keystream);
    keystream
}

// ---------------------------------------------------------------------------
// `PasswordUtil`
// ---------------------------------------------------------------------------

/// Port of `com.healthmarketscience.jackcess.crypt.PasswordUtil`: adds,
/// changes and removes the database password on a database file.
///
/// Only jet 4 databases are supported, and the other formats throw rather than
/// write something ms access cannot open.
pub struct PasswordUtil;

impl PasswordUtil {
    /// `PasswordUtil.setDatabasePassword(srcFile, dstFile, password)`: writes a
    /// copy of the given database with the given password, leaving the source
    /// alone.
    pub fn set_database_password(src_file: &Path, dst_file: &Path, password: &str) -> Result<()> {
        Self::require_password(password)?;
        DbFileRewriter::rewrite_to(src_file, dst_file, |src, dst| {
            rewrite_password(src, dst, Some(password))
        })
    }

    /// `PasswordUtil.setDatabasePassword(dbFile, password)`: sets the password
    /// on the given database, replacing the file with the result.
    pub fn set_database_password_in_place(db_file: &Path, password: &str) -> Result<()> {
        Self::require_password(password)?;
        DbFileRewriter::rewrite_in_place(db_file, |src, dst| {
            rewrite_password(src, dst, Some(password))
        })
    }

    /// `PasswordUtil.setDatabasePassword(db, password)`: sets the password on
    /// an open database, which must be open for writing.  The caller holds the
    /// file, so this form is not safe from an interruption part way through.
    pub fn set_database_password_on(db: &mut Database, password: &str) -> Result<()> {
        Self::require_password(password)?;
        JetPasswordHandler::set_password(db, Some(password))
    }

    /// `PasswordUtil.removeDatabasePassword(srcFile, dstFile)`: writes a copy
    /// of the given database with no password.
    pub fn remove_database_password(src_file: &Path, dst_file: &Path) -> Result<()> {
        DbFileRewriter::rewrite_to(src_file, dst_file, |src, dst| {
            rewrite_password(src, dst, None)
        })
    }

    /// `PasswordUtil.removeDatabasePassword(dbFile)`: removes the password,
    /// replacing the file with the result.  Does nothing if it has none.
    pub fn remove_database_password_in_place(db_file: &Path) -> Result<()> {
        DbFileRewriter::rewrite_in_place(db_file, |src, dst| rewrite_password(src, dst, None))
    }

    /// `PasswordUtil.removeDatabasePassword(db)`: removes the password from an
    /// open database, which must be open for writing.
    pub fn remove_database_password_on(db: &mut Database) -> Result<()> {
        JetPasswordHandler::set_password(db, None)
    }

    fn require_password(password: &str) -> Result<()> {
        if password.is_empty() {
            return Err(Error::IllegalArgument(
                "Password may not be empty, use removeDatabasePassword instead".into(),
            ));
        }
        Ok(())
    }

    /// The format is read from the file rather than from an open database,
    /// because a format this cannot write may also need a codec to open at
    /// all, which would report a missing codec instead of the real problem.
    fn require_jet4(db_file: &Path) -> Result<()> {
        let mut file = std::fs::File::open(db_file)?;
        let format = read_file_format(&mut file)?;
        JetPasswordHandler::validate_format(&format)
    }
}

/// Writes a jet 3 database password into a copy of a database, the way
/// `JetPasswordHandler.writePasswordField` would if jet 3 were writable.
///
/// This is test-side tooling for the verification harness
/// (`src/bin/jet3crypt.rs`), not part of the Java API: `PasswordUtil` and
/// [`JetPasswordHandler`] refuse every format but jet 4 exactly like
/// jackcess-encrypt ("Modifying the database password is only supported for
/// jet 4 databases"), and this function is what writes the field instead.
///
/// The jet 3 field needs no extra mask: `DatabaseImpl.getPasswordMask` returns
/// `null` for a format whose `OFFSET_HEADER_DATE` is negative, so the bytes
/// are stored exactly as the database charset encodes them, zero padded to
/// [`JetFormat::size_password`].  `None` (or an empty password) zeroes the
/// field, which is how a jet database says it has no password.
///
/// Only the 20 byte password field is written.  A real jet 3 password change
/// would also re-mask every SID with the key folded out of the new field (the
/// header date is part of that fold, and jet 3 has none), so a database
/// produced here is readable by jackcess and by this port, but ms access may
/// still refuse to open it.  That limitation is the reason the Java library
/// refuses jet 3 password changes in the first place.
///
/// The source must be a plain (not page encoded) jet 3 database: the copy is
/// opened to read the charset the password is stored in.
pub fn set_jet3_password_file(
    src_file: &Path,
    dst_file: &Path,
    password: Option<&str>,
) -> Result<()> {
    DbFileRewriter::rewrite_to(src_file, dst_file, |src, dst| {
        let mut file = std::fs::File::open(src)?;
        let format = read_file_format(&mut file)?;
        if format.name != "VERSION_3" {
            return Err(Error::Unsupported(format!(
                "Setting a jet 3 password is only supported for jet 3 \
                 databases, not {}",
                format.name
            )));
        }

        std::fs::copy(src, dst)?;

        // the password is stored in the database charset, which jet 3 names by
        // the code page in page 0.  Opening the copy reads that the same way a
        // reader of the result will.
        let charset = Database::open_path_read_only(dst)?.charset;

        let field = JetPasswordHandler::encode_password_field(&format, charset, password)?;

        let mut page = vec![0u8; format.page_size];
        let mut out = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(dst)?;
        read_page(&mut out, &mut page, 0, format.page_size)?;
        let mut header = ByteBuf::wrap_le(page.clone());
        apply_header_mask(&mut header, &format)?;
        // jet 3 has no creation date mask (`getPasswordMask` returns null), so
        // the field goes in as encoded, like `writePasswordField`
        header.put_bytes_at(format.offset_password, &field)?;
        apply_header_mask(&mut header, &format)?;
        page.copy_from_slice(header.bytes());
        write_page(&mut out, &page, 0, format.page_size)?;
        Ok(())
    })
}

/// Copies the database and changes the password in the copy
/// (`PasswordUtil.rewritePassword`).
fn rewrite_password(src_file: &Path, dst_file: &Path, password: Option<&str>) -> Result<()> {
    PasswordUtil::require_jet4(src_file)?;

    std::fs::copy(src_file, dst_file)?;

    let mut db = Database::open_path(dst_file)?;
    JetPasswordHandler::set_password(&mut db, password)?;
    db.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Fixtures derived from jackcess 5.0.4 / jackcess-encrypt 5.0.2 itself:
    //! `java-src/CryptFixture.java` builds the same synthetic header page and
    //! prints the values below (see `tools/verify-crypt.ps1`).  The Java
    //! routines are `SidRemasker.createSidKey`, `SidRemasker.createKeystream`,
    //! `DatabaseImpl.getPasswordMask` and `JetPasswordHandler
    //! .writePasswordField`.

    use super::*;

    /// The synthetic header page `CryptFixture.java` uses: every byte is
    /// `i * 7 + 3`, the 40 byte password field at 66 holds "Pa55" as UTF-16LE
    /// zero padded, and the creation date at 114 is 45000.5.
    fn fixture_header_page() -> ByteBuf {
        let format = JetFormat::version_4();
        let mut page = ByteBuf::allocate(format.page_size);
        for i in 0..format.page_size {
            page.put_u8_at(i, (i.wrapping_mul(7).wrapping_add(3)) as u8)
                .unwrap();
        }
        for i in 0..format.size_password {
            page.put_u8_at(format.offset_password + i, 0).unwrap();
        }
        for (i, b) in "Pa55"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .enumerate()
        {
            page.put_u8_at(format.offset_password + i, b).unwrap();
        }
        page.put_f64_at(format.offset_header_date as usize, 45000.5)
            .unwrap();
        page
    }

    /// `SidRemasker.createSidKey` = 0x1b35755e for the fixture header page.
    #[test]
    fn sid_key_matches_the_java_fixture() {
        let format = JetFormat::version_4();
        let page = fixture_header_page();
        assert_eq!(
            SidRemasker::create_sid_key(&page, &format).unwrap(),
            0x1b35_755e
        );
    }

    /// `SidRemasker.createKeystream(key, 8)` = 37 7c aa e8 75 61 0a 0b, and the
    /// xor of two keystreams (the remask pad) for a 3 byte SID = 80 ff 7c.
    #[test]
    fn sid_keystream_and_remask_pad_match_the_java_fixture() {
        let format = JetFormat::version_4();
        let page = fixture_header_page();
        let key = SidRemasker::create_sid_key(&page, &format).unwrap();
        assert_eq!(
            create_keystream(key, 8),
            vec![0x37, 0x7c, 0xaa, 0xe8, 0x75, 0x61, 0x0a, 0x0b]
        );

        let mut sid = [0u8; 3];
        RemaskPad::new(key, 0x1020_3040).apply(&mut sid);
        assert_eq!(sid, [0x80, 0xff, 0x7c]);
    }

    /// `DatabaseImpl.getPasswordMask` for the fixture date 45000.5 is
    /// `(int)45000.5 = 0xafc8`, little endian: c8 af 00 00.
    #[test]
    fn password_mask_matches_the_java_fixture() {
        let format = JetFormat::version_4();
        let page = fixture_header_page();
        assert_eq!(
            get_password_mask(&page, &format).unwrap(),
            Some([0xc8, 0xaf, 0x00, 0x00])
        );
        // jet 3 has no creation date mask
        let mut v3 = format.clone();
        v3.offset_header_date = -1;
        assert_eq!(get_password_mask(&page, &v3).unwrap(), None);
    }

    /// The stored (masked) password field `JetPasswordHandler
    /// .writePasswordField` produces for "Pa55" on the fixture header page.
    #[test]
    fn stored_password_field_matches_the_java_fixture() {
        let format = JetFormat::version_4();
        let mut page = fixture_header_page();
        let field = JetPasswordHandler::encode_password_field(
            &format,
            crate::format::Charset::Utf16Le,
            Some("Pa55"),
        )
        .unwrap();
        JetPasswordHandler::write_password_field(&mut page, &format, &field).unwrap();
        let stored = page
            .get_bytes_at(format.offset_password, format.size_password)
            .unwrap();
        let expected: Vec<u8> = vec![
            0x98, 0xaf, 0x61, 0x00, 0xfd, 0xaf, 0x35, 0x00, 0xc8, 0xaf, 0x00, 0x00, 0xc8, 0xaf,
            0x00, 0x00, 0xc8, 0xaf, 0x00, 0x00, 0xc8, 0xaf, 0x00, 0x00, 0xc8, 0xaf, 0x00, 0x00,
            0xc8, 0xaf, 0x00, 0x00, 0xc8, 0xaf, 0x00, 0x00, 0xc8, 0xaf, 0x00, 0x00,
        ];
        assert_eq!(stored, expected);
    }

    /// `createPasswordField` encodes the password with the database charset
    /// (UTF-16LE for jet 4), zero pads the field, and rejects a password which
    /// does not fit with the Java message.
    #[test]
    fn password_field_encoding_and_length_check() {
        let format = JetFormat::version_4();
        let field = JetPasswordHandler::encode_password_field(
            &format,
            crate::format::Charset::Utf16Le,
            Some("abc"),
        )
        .unwrap();
        assert_eq!(field.len(), 40);
        assert_eq!(
            &field[..8],
            &[0x61, 0x00, 0x62, 0x00, 0x63, 0x00, 0x00, 0x00]
        );
        assert!(field[8..].iter().all(|&b| b == 0));

        let empty = JetPasswordHandler::encode_password_field(
            &format,
            crate::format::Charset::Utf16Le,
            None,
        )
        .unwrap();
        assert!(empty.iter().all(|&b| b == 0));

        let long = "x".repeat(21);
        let err = JetPasswordHandler::encode_password_field(
            &format,
            crate::format::Charset::Utf16Le,
            Some(&long),
        )
        .unwrap_err();
        let msg = format!("{}", err);
        assert!(
            msg.contains("Password encodes to 42 bytes, which is longer than the maximum of 40"),
            "unexpected message: {}",
            msg
        );
    }

    /// `JetPasswordHandler.validateFormat` refuses every format but jet 4 with
    /// the Java message.
    #[test]
    fn password_requires_jet_4() {
        let err = JetPasswordHandler::validate_format(&JetFormat::version_3()).unwrap_err();
        assert_eq!(
            format!("{}", err),
            "unsupported: Modifying the database password is only supported for jet 4 \
             databases, not VERSION_3"
        );
        assert!(JetPasswordHandler::validate_format(&JetFormat::version_4()).is_ok());
    }

    /// The jet 3 password field: 20 bytes, the database charset's bytes zero
    /// padded, and no creation date mask (`getPasswordMask` is null for jet 3,
    /// whose `OFFSET_HEADER_DATE` is -1).  `Database.getDatabasePassword`
    /// reads exactly this field with `Charset.decode` and trims the trailing
    /// nulls, so the round trip below is that read.
    #[test]
    fn jet3_password_field_has_no_date_mask_and_reads_back() {
        let format = JetFormat::version_3();
        assert_eq!(format.size_password, 20);
        assert_eq!(format.offset_password, 66);
        assert!(format.offset_header_date < 0);

        // a demasked page 0 whose password field starts empty
        let mut page = ByteBuf::allocate(format.page_size);
        for i in 0..format.page_size {
            page.put_u8_at(i, (i.wrapping_mul(7).wrapping_add(3)) as u8)
                .unwrap();
        }
        for i in 0..format.size_password {
            page.put_u8_at(format.offset_password + i, 0).unwrap();
        }

        let charset = crate::format::Charset::Cp1252;
        let field =
            JetPasswordHandler::encode_password_field(&format, charset, Some("Pa55")).unwrap();
        assert_eq!(field.len(), 20);
        assert_eq!(&field[..4], b"Pa55");
        assert!(field[4..].iter().all(|&b| b == 0));

        let after_field = page
            .get_bytes_at(
                format.offset_password + format.size_password,
                format.size_password,
            )
            .unwrap();

        // `writePasswordField` applies the creation date mask only when the
        // format has one, so on jet 3 the bytes are stored as encoded
        JetPasswordHandler::write_password_field(&mut page, &format, &field).unwrap();
        assert_eq!(
            page.get_bytes_at(format.offset_password, format.size_password)
                .unwrap(),
            field
        );
        assert_eq!(get_password_mask(&page, &format).unwrap(), None);

        // the password region is the field followed by the next 20 bytes, with
        // no mask applied to either
        let region = read_password_region(&page, &format).unwrap();
        assert_eq!(region.len(), 40);
        assert_eq!(&region[..20], &field[..]);
        assert_eq!(&region[20..], &after_field[..]);

        // the read `get_database_password` performs: charset decode, then cut
        // at the first null code unit
        let units = charset.decode(&region[..20]);
        let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
        assert_eq!(String::from_utf16_lossy(&units[..end]), "Pa55");

        // an empty or missing password zeroes the field
        for password in [None, Some("")] {
            let empty =
                JetPasswordHandler::encode_password_field(&format, charset, password).unwrap();
            assert_eq!(empty, vec![0u8; 20]);
        }

        // 20 bytes is the jet 3 maximum; 21 is refused with the Java message
        let long = "x".repeat(21);
        let err =
            JetPasswordHandler::encode_password_field(&format, charset, Some(&long)).unwrap_err();
        assert_eq!(
            format!("{}", err),
            "illegal argument: Password encodes to 21 bytes, which is longer than the \
             maximum of 20"
        );
    }
}
