//! Port of `com.healthmarketscience.jackcess.crypt.impl.JetEncodingHandler`.
//!
//! Jet "encoding" is what ms access called "Encode/Decode Database" and
//! removed after access 2003.  Every page above page 0 is run through RC4
//! under the encoding key xored with the page number; page 0 is never encoded
//! and holds the key in the clear at [`JetFormat::offset_encoding_key`].  The
//! transform is its own inverse, so one routine does both directions.
//!
//! [`JetEncodingHandler`] only alters jet 4 databases, exactly like the Java
//! handler: altering the encoding rewrites the whole database, and jackcess
//! does not write jet 3 files.  Reading is not limited: an encoded jet 3
//! database opens through [`crate::crypt::CryptCodecProvider`] because the
//! transform is the same (only the page size and the page 0 layout come from
//! the jet 3 format).  For the verification harness, which has to produce an
//! encoded Access 97 file that neither implementation can write through its
//! public API, [`recode_jet3`] performs the same recode on a `VERSION_3`
//! database.  It is deliberately not reachable through [`JetEncodingHandler`],
//! so the public behaviour stays the Java one: adding or removing the encoding
//! of a jet 3 database is refused.
//!
//! The Java handler writes into a destination file which already exists
//! (`FileChannel.open(dst, WRITE)` does not create it).  The port goes through
//! [`DbFileRewriter`] like the Java `EncryptionUtil` entry points do: it builds
//! the result in a temporary file beside the destination and moves it into
//! place, so the destination need not exist and an interrupted operation leaves
//! it as it was.  The bytes written are the same either way.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::{apply_header_mask, create_encoding_key};
use crate::crypt::rewriter::{count_pages, read_page, write_page, DbFileRewriter};
use crate::error::{Error, Result};
use crate::format::JetFormat;
use rc4::{KeyInit, Rc4, StreamCipher};
use std::path::Path;

/// Port of `JetEncodingHandler`.
pub struct JetEncodingHandler;

impl JetEncodingHandler {
    /// `JetEncodingHandler.addEncoding(src, dst)`: copies the database to the
    /// destination, adding jet encoding under a freshly generated key.
    pub fn add_encoding(src_file: &Path, dst_file: &Path) -> Result<()> {
        encode_file(src_file, dst_file, EncodeKey::Random)
    }

    /// `JetEncodingHandler.addEncoding(src, dst, key)`: copies the database to
    /// the destination, adding jet encoding under the given key, which is for
    /// reproducing a known file and must not be zero.
    pub fn add_encoding_with_key(src_file: &Path, dst_file: &Path, key: i32) -> Result<()> {
        encode_file(src_file, dst_file, EncodeKey::Explicit(key))
    }

    /// `JetEncodingHandler.removeEncoding(src, dst)`: copies the database to
    /// the destination, removing its jet encoding.
    pub fn remove_encoding(src_file: &Path, dst_file: &Path) -> Result<()> {
        decode_file(src_file, dst_file)
    }

    /// Adds encoding to the given database, replacing it with the result.
    pub fn add_encoding_in_place(db_file: &Path) -> Result<()> {
        encode_file_in_place(db_file, EncodeKey::Random)
    }

    /// Adds encoding under the given key, replacing the database.
    pub fn add_encoding_with_key_in_place(db_file: &Path, key: i32) -> Result<()> {
        encode_file_in_place(db_file, EncodeKey::Explicit(key))
    }

    /// Removes the encoding from the given database, replacing it.
    pub fn remove_encoding_in_place(db_file: &Path) -> Result<()> {
        DbFileRewriter::rewrite_in_place(db_file, |src, dst| recode(src, dst, false, 0))
    }
}

/// The key an `encode_file` call adds the encoding under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodeKey {
    /// A freshly generated key (`BaseCryptCodecHandler.createEncodingKey`).
    Random,
    /// A fixed key, for reproducing a known file.
    Explicit(i32),
}

/// Adds jet encoding to a copy of `src`, writing it to `dst`, under the given
/// key.  This is the explicit-key entry point (`encode_file(src, dst, key)`).
pub fn encode_file(src: &Path, dst: &Path, key: EncodeKey) -> Result<()> {
    if let EncodeKey::Explicit(k) = key {
        if k == 0 {
            return Err(Error::IllegalArgument(
                "Encoding key may not be zero".into(),
            ));
        }
    }
    DbFileRewriter::rewrite_to(src, dst, |src, dst| {
        let new_key = match key {
            EncodeKey::Random => create_encoding_key(),
            EncodeKey::Explicit(k) => k,
        };
        recode(src, dst, true, new_key)
    })
}

/// Convenience wrapper for [`encode_file`] with an explicit key.
pub fn encode_file_with_key(src: &Path, dst: &Path, key: i32) -> Result<()> {
    encode_file(src, dst, EncodeKey::Explicit(key))
}

/// Copies the encoded `src` to `dst`, removing the encoding.
pub fn decode_file(src: &Path, dst: &Path) -> Result<()> {
    DbFileRewriter::rewrite_to(src, dst, |src, dst| recode(src, dst, false, 0))
}

/// Adds encoding to the database file, replacing it, under the given key.
pub fn encode_file_in_place(db_file: &Path, key: EncodeKey) -> Result<()> {
    if let EncodeKey::Explicit(k) = key {
        if k == 0 {
            return Err(Error::IllegalArgument(
                "Encoding key may not be zero".into(),
            ));
        }
    }
    DbFileRewriter::rewrite_in_place(db_file, |src, dst| {
        let new_key = match key {
            EncodeKey::Random => create_encoding_key(),
            EncodeKey::Explicit(k) => k,
        };
        recode(src, dst, true, new_key)
    })
}

/// `JetEncodingHandler.recode`: adds or removes the encoding of the source
/// database while copying it to the destination.
fn recode(src_file: &Path, dst_file: &Path, add: bool, new_key: i32) -> Result<()> {
    recode_pages(src_file, dst_file, add, new_key, validate_format)
}

/// The jet 3 form of `JetEncodingHandler.recode`, for the verification harness
/// (`src/bin/jet3crypt.rs`) and the unit tests.  It exists because neither
/// jackcess-encrypt nor this port writes jet 3 files through the public API
/// ([`JetEncodingHandler::add_encoding`] and
/// [`JetEncodingHandler::remove_encoding`] refuse, with the Java message), yet
/// a real encoded Access 97 database is needed to exercise the read path.
///
/// The work is the same one `recode` does: page 0 has the header mask removed,
/// its encoding key at [`JetFormat::offset_encoding_key`] is set (adding) or
/// cleared (removing), the mask is put back and the page is written, and every
/// page above 0 goes through [`crypt_page`] under the key xored with the page
/// number.  The only jet 3 specific parts are the 2048 byte page size and the
/// jet 3 header layout, which both come from the format.
///
/// `new_key` may be zero when `add` is `false` (removing); adding under a zero
/// key is refused, like the Java handler.
pub fn recode_jet3(src_file: &Path, dst_file: &Path, add: bool, new_key: i32) -> Result<()> {
    if add && new_key == 0 {
        return Err(Error::IllegalArgument(
            "Encoding key may not be zero".into(),
        ));
    }
    // the destination need not exist, and an interrupted recode leaves it as
    // it was, like the other path based entry points here
    DbFileRewriter::rewrite_to(src_file, dst_file, |src, dst| {
        recode_pages(src, dst, add, new_key, validate_jet3_format)
    })
}

/// The page loop shared by [`recode`] and [`recode_jet3`]; the validator picks
/// the format the caller is allowed to rewrite.
fn recode_pages(
    src_file: &Path,
    dst_file: &Path,
    add: bool,
    new_key: i32,
    validate: fn(&JetFormat) -> Result<()>,
) -> Result<()> {
    let mut src = std::fs::OpenOptions::new().read(true).open(src_file)?;
    let mut dst = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dst_file)?;

    let format = crate::crypt::rewriter::read_file_format(&mut src)?;
    validate(&format)?;

    let page_size = format.page_size;
    let page_count = count_pages(src_file, page_size)?;

    let mut page = vec![0u8; page_size];
    read_page(&mut src, &mut page, 0, page_size)?;
    let mut header = ByteBuf::wrap_le(page.clone());
    apply_header_mask(&mut header, &format)?;
    let mut key = header.get_i32_at(format.offset_encoding_key)?;
    if add {
        if key != 0 {
            return Err(Error::IllegalState(
                "Database is already encoded, cannot add encoding".into(),
            ));
        }
        key = new_key;
    } else if key == 0 {
        return Err(Error::IllegalState(
            "Database is not encoded, cannot remove encoding".into(),
        ));
    }
    header.put_i32_at(format.offset_encoding_key, if add { key } else { 0 })?;
    apply_header_mask(&mut header, &format)?;
    // page 0 carries the key, and is written first so that a reader of the
    // destination sees the state its pages are in
    page.copy_from_slice(header.bytes());
    write_page(&mut dst, &page, 0, page_size)?;

    for page_number in 1..page_count {
        read_page(&mut src, &mut page, page_number as i32, page_size)?;
        crypt_page(&mut page, key, page_number as i32);
        write_page(&mut dst, &page, page_number as i32, page_size)?;
    }
    Ok(())
}

/// `JetEncodingHandler.validateFormat`: jet 3 is refused in both directions.
fn validate_format(format: &JetFormat) -> Result<()> {
    if format.name != "VERSION_4" {
        return Err(Error::Unsupported(format!(
            "Jet encoding is only supported for jet 4 databases, not {}",
            format.name
        )));
    }
    Ok(())
}

/// The format check of [`recode_jet3`]: the reverse of [`validate_format`].
fn validate_jet3_format(format: &JetFormat) -> Result<()> {
    if format.name != "VERSION_3" {
        return Err(Error::Unsupported(format!(
            "Jet 3 recoding is only supported for jet 3 databases, not {}",
            format.name
        )));
    }
    Ok(())
}

/// `JetEncodingHandler.cryptPage`: encodes or decodes one page, which are the
/// same operation.
pub fn crypt_page(page: &mut [u8], key: i32, page_number: i32) {
    let page_key = (key as u32 ^ page_number as u32).to_le_bytes();
    let mut cipher = Rc4::new_from_slice(&page_key).expect("4 byte keys are supported");
    cipher.apply_keystream(page);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_key_is_rejected() {
        let err = encode_file(
            Path::new("does-not-exist"),
            Path::new("does-not-exist-out"),
            EncodeKey::Explicit(0),
        )
        .unwrap_err();
        assert!(format!("{}", err).contains("Encoding key may not be zero"));
    }

    #[test]
    fn crypt_page_is_its_own_inverse() {
        let page: Vec<u8> = (0..64u8).collect();
        let mut buf = page.clone();
        crypt_page(&mut buf, 0x1234_abcd, 3);
        assert_ne!(buf, page);
        crypt_page(&mut buf, 0x1234_abcd, 3);
        assert_eq!(buf, page);
    }

    /// The jet 3 page is 2048 bytes, and `crypt_page` covers the whole page:
    /// it is its own inverse at jet 3 page numbers, and the page 0 key is
    /// untouched by it (page 0 is never encoded).
    #[test]
    fn jet3_crypt_page_is_its_own_inverse() {
        let page: Vec<u8> = (0..2048usize)
            .map(|i| i.wrapping_mul(31).wrapping_add(5) as u8)
            .collect();
        for page_number in [1i32, 2, 57, 0x7fff] {
            let mut buf = page.clone();
            crypt_page(&mut buf, 0x1234_abcd, page_number);
            assert_ne!(buf, page, "page {} was not transformed", page_number);
            crypt_page(&mut buf, 0x1234_abcd, page_number);
            assert_eq!(buf, page, "page {} did not invert", page_number);
        }
    }

    /// The jet 3 page transform, pinned against `java-src/Jet3CryptGen.java`'s
    /// `fixtures` mode (which calls the library's own private
    /// `JetEncodingHandler.cryptPage`): the synthetic page whose byte `i` is
    /// `i * 31 + 5`, transformed under key `0x1234abcd` at page 1 and page
    /// 1000.  `tools/verify-jet3-crypt.ps1` compares the two tools' `fixtures`
    /// output as well.
    #[test]
    fn jet3_page_transform_matches_the_java_fixture() {
        let head = [
            0xe8, 0xe8, 0x54, 0x24, 0xa0, 0x86, 0x90, 0x0f, 0x74, 0xc7, 0xd7, 0x6e, 0x51, 0x39,
            0x22, 0x6a,
        ];
        let tail = [
            0xed, 0xc1, 0x3a, 0x83, 0xa2, 0x71, 0x5a, 0xca, 0x0e, 0x02, 0xb0, 0xa7, 0x38, 0x47,
            0xbb, 0x08,
        ];
        let head_1000 = [
            0x9c, 0x7f, 0x23, 0xa6, 0x4f, 0x35, 0xf8, 0x0f, 0x46, 0x8f, 0x55, 0x50, 0x06, 0x75,
            0xb4, 0xdf,
        ];
        let tail_1000 = [
            0xc8, 0x4a, 0x38, 0x11, 0x77, 0x5e, 0x39, 0xfc, 0xba, 0x4b, 0xb3, 0x42, 0x91, 0x16,
            0x44, 0x7f,
        ];

        for (page_number, expected_head, expected_tail) in
            [(1i32, head, tail), (1000i32, head_1000, tail_1000)]
        {
            let mut page: Vec<u8> = (0..2048usize)
                .map(|i| i.wrapping_mul(31).wrapping_add(5) as u8)
                .collect();
            crypt_page(&mut page, 0x1234_abcd, page_number);
            assert_eq!(&page[..16], &expected_head, "page {}", page_number);
            assert_eq!(&page[2032..], &expected_tail, "page {}", page_number);
        }
    }

    /// The jet 3 layout the encoder relies on: the 4 byte encoding key at
    /// offset 62 sits inside the 126 byte masked header, so page 0 has to have
    /// the mask removed before the key is read and put back before it is
    /// written.  This is exactly what `recode_jet3` and the codec handler do.
    #[test]
    fn jet3_encoding_key_lives_in_the_masked_header() {
        let format = JetFormat::version_3();
        assert_eq!(format.page_size, 2048);
        assert_eq!(format.offset_encoding_key, 62);
        assert_eq!(format.header_mask.len(), 126);
        assert_eq!(format.offset_masked_header, 24);
        assert!(
            format.offset_encoding_key >= format.offset_masked_header
                && format.offset_encoding_key + 4
                    <= format.offset_masked_header + format.header_mask.len()
        );

        let mut page = ByteBuf::allocate(format.page_size);
        // the masked bytes on disk
        for (i, m) in format.header_mask.iter().enumerate() {
            page.put_u8_at(format.offset_masked_header + i, *m ^ 0x5a)
                .unwrap();
        }
        page.put_i32_at(format.offset_encoding_key, 0x0bad_f00d)
            .unwrap();
        // adding the mask is what writing page 0 does; removing it is what
        // reading page 0 does
        apply_header_mask(&mut page, &format).unwrap();
        assert_ne!(
            page.get_i32_at(format.offset_encoding_key).unwrap(),
            0x0bad_f00d
        );
        apply_header_mask(&mut page, &format).unwrap();
        assert_eq!(
            page.get_i32_at(format.offset_encoding_key).unwrap(),
            0x0bad_f00d
        );
    }

    /// `recode_jet3` only accepts a jet 3 file (the reverse of the public
    /// handler's jet 4 check) and refuses a zero key when adding.
    #[test]
    fn recode_jet3_refuses_the_wrong_format_and_a_zero_key() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-tmp");
        let _ = std::fs::create_dir_all(&dir);
        let not_jet3 = dir.join("recode_jet3_not_jet3.mdb");
        // the format is read from the version byte, so a short synthetic file
        // is enough to exercise the check
        let mut header = vec![0u8; 2048];
        header[20] = 1; // VERSION_4
        std::fs::write(&not_jet3, &header).unwrap();

        let err = recode_jet3(&not_jet3, &dir.join("out.mdb"), true, 1).unwrap_err();
        assert_eq!(
            format!("{}", err),
            "unsupported: Jet 3 recoding is only supported for jet 3 databases, \
             not VERSION_4"
        );

        let mut v3 = vec![0u8; 2048];
        v3[20] = 0;
        std::fs::write(&not_jet3, &v3).unwrap();
        let err = recode_jet3(&not_jet3, &dir.join("out.mdb"), true, 0).unwrap_err();
        assert_eq!(
            format!("{}", err),
            "illegal argument: Encoding key may not be zero"
        );
    }
}
