//! Port of `com.healthmarketscience.jackcess.crypt.impl.OfficeEncryptionHandler`:
//! adds agile encryption to, and removes any readable encryption from, an
//! `.accdb` database file.
//!
//! Page 0 is never encrypted and carries three things which have to change:
//! the encoding key, which office uses as a per file salt, the password field,
//! which holds the low byte of that key repeated, and the `EncryptionInfo`
//! structure.  Clearing the password field moves the key which masks every SID,
//! so the SIDs follow ([`SidRemasker`]).
//!
//! Removing the encryption is the read path plus page 0, so it works for every
//! scheme the codec can open; adding it writes agile, the scheme a current ms
//! access writes.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::{
    apply_header_mask, create_encoding_key, get_password_mask, read_header_page,
};
use crate::crypt::office::agile::{AgileEncryptionConfig, AgileEncryptionWriter, AgileKeyMaterial};
use crate::crypt::office::provider::CRYPT_STRUCTURE_OFFSET;
use crate::crypt::password::SidRemasker;
use crate::crypt::rewriter::{count_pages, read_file_format, read_page, write_page};
use crate::crypt::CryptCodecProvider;
use crate::database::Database;
use crate::error::{Error, Result};
use crate::format::{JetFormat, OFFSET_VERSION};
use std::path::Path;

/// The page 0 region the `EncryptionInfo` structure lives in.  The structure
/// carries its own length and is not the same size in every scheme, so the
/// region is bounded by what follows it rather than by any one structure.
const ENCRYPTION_INFO_END: usize = 0xE00;

/// The version agile encryption declares, and the value which follows it.
const AGILE_VERSION_MAJOR: u16 = 4;
const AGILE_VERSION_MINOR: u16 = 4;
const AGILE_RESERVED: u32 = 0x40;

/// The access 2010 engine version agile needs.
const CODE_VERSION_2010: u8 = 0x3;
const SUB_VERSION_2010: u8 = 0x1;

/// Port of `OfficeEncryptionHandler`.
pub struct OfficeEncryptionHandler;

impl OfficeEncryptionHandler {
    /// `OfficeEncryptionHandler.addEncryption(srcFile, dstFile, password,
    /// config)`: copies the plain database to the destination, encrypted with
    /// agile encryption under the given password.
    pub fn add_encryption(
        src_file: &Path,
        dst_file: &Path,
        password: &str,
        config: &AgileEncryptionConfig,
    ) -> Result<()> {
        if password.is_empty() {
            return Err(Error::IllegalArgument("Password may not be empty".into()));
        }

        // page 0 is never encrypted, so the state is readable before the
        // database is opened.  Without this, opening an encrypted database with
        // no codec provider fails first and reports a missing codec rather
        // than the real problem
        require_plain(src_file)?;

        let old_sid_key = write_encrypted(src_file, dst_file, password, config, None, None)?;
        remask_sids(dst_file, old_sid_key, Some(password))
    }

    /// The deterministic form of [`Self::add_encryption`]: the key material and
    /// the page 0 encoding key are given rather than generated, which is the
    /// only way the (otherwise random) encrypted bytes can be reproduced and
    /// compared.
    pub fn add_encryption_with_key_material(
        src_file: &Path,
        dst_file: &Path,
        password: &str,
        config: &AgileEncryptionConfig,
        material: &AgileKeyMaterial,
        encoding_key: i32,
    ) -> Result<()> {
        if password.is_empty() {
            return Err(Error::IllegalArgument("Password may not be empty".into()));
        }
        require_plain(src_file)?;

        let old_sid_key = write_encrypted(
            src_file,
            dst_file,
            password,
            config,
            Some(material),
            Some(encoding_key),
        )?;
        remask_sids(dst_file, old_sid_key, Some(password))
    }

    /// `OfficeEncryptionHandler.removeEncryption(srcFile, dstFile, password)`:
    /// copies the encrypted database to the destination, decrypted.
    pub fn remove_encryption(src_file: &Path, dst_file: &Path, password: &str) -> Result<()> {
        let old_sid_key = write_decrypted(src_file, dst_file, password)?;
        remask_sids(dst_file, old_sid_key, None)
    }

    /// Fails unless the given format is one which carries office encryption.
    pub fn validate_format(format: &JetFormat) -> Result<()> {
        let supported = matches!(
            format.name,
            "VERSION_2007" | "VERSION_2010" | "VERSION_2016" | "VERSION_2019"
        );
        if !supported {
            return Err(Error::Unsupported(format!(
                "Office encryption is only supported for accdb databases, not {}",
                format.name
            )));
        }
        Ok(())
    }

    /// Returns whether the given demasked page 0 describes a plain database.
    pub fn is_plain(page: &ByteBuf, format: &JetFormat) -> Result<bool> {
        Ok(page.get_i32_at(format.offset_encoding_key)? == 0
            && page.get_u16_at(CRYPT_STRUCTURE_OFFSET)? == 0)
    }
}

/// `OfficeEncryptionHandler.writeEncrypted`: writes the encrypted database and
/// returns the SID key the plain source used, which the destination no longer
/// describes.
fn write_encrypted(
    src_file: &Path,
    dst_file: &Path,
    password: &str,
    config: &AgileEncryptionConfig,
    material: Option<&AgileKeyMaterial>,
    encoding_key: Option<i32>,
) -> Result<i32> {
    let mut db = Database::open_path_read_only(src_file)?;
    let format = db.format.clone();
    OfficeEncryptionHandler::validate_format(&format)?;

    let page_size = format.page_size;
    let page_count = count_pages(src_file, page_size)?;

    let mut page = db.channel.create_page_buffer();
    db.channel.read_page(&mut page, 0)?;
    if !OfficeEncryptionHandler::is_plain(&page, &format)? {
        return Err(Error::IllegalState(
            "Database is already encrypted, cannot add encryption".into(),
        ));
    }
    let old_sid_key = SidRemasker::create_sid_key(&page, &format)?;

    let writer = match material {
        Some(material) => {
            AgileEncryptionWriter::with_key_material(config.clone(), password, material.clone())?
        }
        None => AgileEncryptionWriter::new(config.clone(), password)?,
    };
    let encoding_key = encoding_key.unwrap_or_else(create_encoding_key);
    let key_value = writer.key_value().to_vec();
    let key_data_salt = writer.key_data_salt().to_vec();
    let encoding_key_bytes = encoding_key.to_le_bytes();

    let mut dst = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dst_file)?;

    fill_header_page(&mut page, &format, encoding_key, &writer)?;
    apply_header_mask(&mut page, &format)?;
    write_page(&mut dst, page.bytes(), 0, page_size)?;

    for page_number in 1..page_count {
        page.clear();
        db.channel.read_page(&mut page, page_number as i32)?;
        let block_bytes =
            crate::crypt::codec::apply_page_number(&encoding_key_bytes, 0, page_number as i32);
        AgileEncryptionWriter::encrypt_page(
            config,
            &key_value,
            &key_data_salt,
            &block_bytes,
            &mut page.array_mut()[..page_size],
        )?;
        write_page(&mut dst, page.bytes(), page_number as i32, page_size)?;
    }

    Ok(old_sid_key)
}

/// `OfficeEncryptionHandler.writeDecrypted`: writes the decrypted database and
/// returns the SID key the source used.
fn write_decrypted(src_file: &Path, dst_file: &Path, password: &str) -> Result<i32> {
    let provider = CryptCodecProvider::with_password(password);
    let mut db = Database::open_path_read_only_with_provider(src_file, &provider)?;
    let format = db.format.clone();
    OfficeEncryptionHandler::validate_format(&format)?;

    let page_size = format.page_size;
    let page_count = count_pages(src_file, page_size)?;

    let mut page = db.channel.create_page_buffer();
    // readPage removes the header mask from page 0, so the key folds out of it
    // as it stands
    db.channel.read_page(&mut page, 0)?;
    let old_sid_key = SidRemasker::create_sid_key(&page, &format)?;
    if OfficeEncryptionHandler::is_plain(&page, &format)? {
        return Err(Error::IllegalState(
            "Database is not encrypted, cannot remove encryption".into(),
        ));
    }

    let mut dst = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dst_file)?;

    clear_header_page(&mut page, &format)?;
    apply_header_mask(&mut page, &format)?;
    write_page(&mut dst, page.bytes(), 0, page_size)?;

    for page_number in 1..page_count {
        page.clear();
        db.channel.read_page(&mut page, page_number as i32)?;
        write_page(&mut dst, page.bytes(), page_number as i32, page_size)?;
    }

    Ok(old_sid_key)
}

/// `OfficeEncryptionHandler.fillHeaderPage`: writes the encryption state into
/// the given demasked page 0.
fn fill_header_page(
    page: &mut ByteBuf,
    format: &JetFormat,
    encoding_key: i32,
    writer: &AgileEncryptionWriter,
) -> Result<()> {
    page.put_i32_at(format.offset_encoding_key, encoding_key)?;

    // ms access fills the password field with the low byte of the encoding key,
    // repeated, under the creation date mask
    let low = (encoding_key & 0xFF) as u8;
    let pwd_mask = get_password_mask(page, format)?;
    for i in 0..format.size_password {
        let mut b = low;
        if let Some(mask) = &pwd_mask {
            b ^= mask[i % mask.len()];
        }
        page.put_u8_at(format.offset_password + i, b)?;
    }

    let xml = writer.create_descriptor()?;
    let len = 8 + xml.len();
    if CRYPT_STRUCTURE_OFFSET + 2 + len > ENCRYPTION_INFO_END {
        return Err(Error::IllegalState(format!(
            "Encryption descriptor is {} bytes, which does not fit",
            len
        )));
    }
    page.clear_range(CRYPT_STRUCTURE_OFFSET, ENCRYPTION_INFO_END)?;
    page.put_u16_at(CRYPT_STRUCTURE_OFFSET, len as u16)?;
    page.put_u16_at(CRYPT_STRUCTURE_OFFSET + 2, AGILE_VERSION_MAJOR)?;
    page.put_u16_at(CRYPT_STRUCTURE_OFFSET + 4, AGILE_VERSION_MINOR)?;
    page.put_u32_at(CRYPT_STRUCTURE_OFFSET + 6, AGILE_RESERVED)?;
    page.put_bytes_at(CRYPT_STRUCTURE_OFFSET + 10, &xml)?;

    // agile needs the 2010 engine, so the version is raised to match.  A
    // database which already says something later keeps it
    if page.get_u8_at(OFFSET_VERSION)? < CODE_VERSION_2010 {
        page.put_u8_at(OFFSET_VERSION, CODE_VERSION_2010)?;
        page.put_u8_at(OFFSET_VERSION + 1, SUB_VERSION_2010)?;
    }
    Ok(())
}

/// `OfficeEncryptionHandler.clearHeaderPage`: removes the encryption state
/// from the given demasked page 0.
fn clear_header_page(page: &mut ByteBuf, format: &JetFormat) -> Result<()> {
    page.put_i32_at(format.offset_encoding_key, 0)?;

    // the password field carries a second mask built from the creation date, so
    // an empty field is that mask rather than zeroes
    let pwd_mask = get_password_mask(page, format)?;
    for i in 0..format.size_password {
        page.put_u8_at(
            format.offset_password + i,
            pwd_mask.map(|mask| mask[i % mask.len()]).unwrap_or(0),
        )?;
    }

    page.clear_range(CRYPT_STRUCTURE_OFFSET, ENCRYPTION_INFO_END)?;
    Ok(())
}

/// `OfficeEncryptionHandler.remaskSids`: converts every SID to the key the
/// destination now describes, on the destination, once its pages are readable.
fn remask_sids(dst_file: &Path, old_sid_key: i32, password: Option<&str>) -> Result<()> {
    let mut db = match password {
        Some(password) => Database::open_path_with_password(dst_file, Some(password))?,
        None => Database::open_path(dst_file)?,
    };
    let format = db.format.clone();
    let header_page = read_header_page(&mut db.channel)?;
    let new_sid_key = SidRemasker::create_sid_key(&header_page, &format)?;

    db.channel.start_write();
    let result = SidRemasker::remask_sids(&mut db, old_sid_key, new_sid_key);
    let finished = db.channel.finish_write();
    result?;
    finished?;
    db.flush()?;
    Ok(())
}

/// `OfficeEncryptionHandler.requirePlain`: fails unless the given database is
/// plain, reading page 0 straight from the file.
fn require_plain(db_file: &Path) -> Result<()> {
    let mut file = std::fs::File::open(db_file)?;
    let format = read_file_format(&mut file)?;
    OfficeEncryptionHandler::validate_format(&format)?;

    let page_size = format.page_size;
    let mut page = vec![0u8; page_size];
    read_page(&mut file, &mut page, 0, page_size)?;
    let mut header = ByteBuf::wrap_le(page);
    apply_header_mask(&mut header, &format)?;

    if !OfficeEncryptionHandler::is_plain(&header, &format)? {
        return Err(Error::IllegalState(
            "Database is already encrypted, cannot add encryption".into(),
        ));
    }
    Ok(())
}
