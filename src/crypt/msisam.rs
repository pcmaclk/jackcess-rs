//! Port of `com.healthmarketscience.jackcess.crypt.impl.MSISAMCryptCodecHandler`
//! (jackcess-encrypt 5.0.2): the page encoding of Microsoft Money (`.mny`)
//! MSISAM databases.
//!
//! MSISAM has two encodings, and the header byte at
//! [`ENCRYPTION_FLAGS_OFFSET`] (page 0, `0x298`) tells them apart:
//!
//! * **New encryption** (the `NEW_ENCRYPTION` bit): the pages are RC4 encoded
//!   under a per page key derived from a digest (MD5, or SHA-1 when
//!   [`USE_SHA1`] is set) of the upper cased password and the 8 byte salt at
//!   [`SALT_OFFSET`] (`0x72`).  The first 4 salt bytes form the base of the
//!   per page key; the 4 bytes at [`CRYPT_CHECK_START`] plus the low byte of
//!   the salt are the encryption of the first 4 salt bytes under the password
//!   digest, which is the password verifier.
//! * **Old encryption** (the bit is clear): this is the jet page transform
//!   with a key derived from page 0 alone (the salt at `0x72`, the password
//!   field, the creation date mask and the engine name) and stopped at page 14.
//!   No password is needed to derive it, which is why a password protected
//!   Money 2001/2002 database opens without one.
//!
//! Both encodings transform **pages 1 through 14 only**
//! ([`MSISAM_MAX_ENCRYPTED_PAGE`]); page 0 is never encoded and every page
//! above 14 is cleartext.  That is the reason a MSISAM database is always
//! reported as encoded by the Java `DefaultCodecProvider`.
//!
//! The salt at `0x72` overlaps the jet 4 *creation date* offset (`0x72`), so
//! the old-encryption key derivation masks the password region with the
//! `(int)creationDate` value exactly like the jet password code does.  The
//! Java handler hard codes these offsets rather than adding them to
//! `JetFormat`; this port keeps the same constants here.

use crate::bytebuf::ByteBuf;
use crate::column::encode_uncompressed_text;
use crate::crypt::codec::{
    apply_page_number, get_password_mask, is_blank_key, read_header_page, read_password_region,
    CodecHandler, KeyCache,
};
use crate::crypt::jet::JetCryptCodecHandler;
use crate::crypt::office::crypto::{fix_to_length, HashAlgorithm};
use crate::error::{Error, Result};
use crate::format::{Charset, JetFormat, LENGTH_ENGINE_NAME, OFFSET_ENGINE_NAME};
use crate::pagechannel::PageChannel;
use rc4::{KeyInit, Rc4, StreamCipher};

/// `MSISAMCryptCodecHandler.SALT_OFFSET`: the 8 byte salt in page 0 (the same
/// offset jet 4 uses for the database creation date).
pub const SALT_OFFSET: usize = 0x72;
/// `MSISAMCryptCodecHandler.CRYPT_CHECK_START`: the password verifier is 4
/// bytes at this offset plus the low byte of the salt.
pub const CRYPT_CHECK_START: usize = 0x2e9;
/// `MSISAMCryptCodecHandler.ENCRYPTION_FLAGS_OFFSET`.
pub const ENCRYPTION_FLAGS_OFFSET: usize = 0x298;
/// `MSISAMCryptCodecHandler.SALT_LENGTH`: the number of salt bytes the key is
/// derived from (the first four of the eight byte salt).
pub const SALT_LENGTH: usize = 0x4;
/// The salt itself is eight bytes long; the Java handler reads it with
/// `ByteUtil.getBytes(buffer, SALT_OFFSET, 8)`.
pub const SALT_BYTES: usize = 8;
/// `MSISAMCryptCodecHandler.PASSWORD_LENGTH`: the password is hashed as a fixed
/// 0x28 byte (zero padded / truncated) buffer.
pub const PASSWORD_LENGTH: usize = 0x28;
/// `MSISAMCryptCodecHandler.USE_SHA1`: use SHA-1 instead of MD5 for the
/// password digest.
pub const USE_SHA1: u8 = 0x20;
/// `MSISAMCryptCodecHandler.PASSWORD_DIGEST_LENGTH`: the digest part of the key
/// (the digest is truncated to this length, so MD5 and SHA-1 produce the same
/// key size).
pub const PASSWORD_DIGEST_LENGTH: usize = 0x10;
/// `MSISAMCryptCodecHandler.MSISAM_MAX_ENCRYPTED_PAGE`: the last encoded page.
pub const MSISAM_MAX_ENCRYPTED_PAGE: i32 = 0xE;
/// `MSISAMCryptCodecHandler.NEW_ENCRYPTION`: page 0 flag bit for the hashed
/// password scheme (otherwise the old jet style key is used).
pub const NEW_ENCRYPTION: u8 = 0x6;
/// `MSISAMCryptCodecHandler.TRAILING_PWD_LEN`: the trailing part of the
/// password region the old key derivation unmasks.
pub const TRAILING_PWD_LEN: usize = 20;

/// The Java error message of a failed password check
/// (`InvalidCredentialsException`).
pub const INCORRECT_PASSWORD: &str = "Incorrect password provided";

/// Port of `com.healthmarketscience.jackcess.crypt.impl.MSISAMCryptCodecHandler`:
/// the RC4 page transform of a modern (hashed password) MSISAM database.
pub struct MsisamCryptCodecHandler {
    /// `_baseHash`: the 16 byte password digest followed by the first 4 salt
    /// bytes (the per page key is this buffer with the page number xored in at
    /// offset 16).
    base_hash: Vec<u8>,
    param_cache: KeyCache,
}

impl MsisamCryptCodecHandler {
    /// `MSISAMCryptCodecHandler.create`: choose the handler for the encoding
    /// the header page names.  A `NEW_ENCRYPTION` header uses the hashed
    /// password key; an old one uses the jet page transform under the
    /// file-only key, stopped at page 14.
    pub fn create_handler(
        channel: &mut PageChannel,
        password: Option<&str>,
        charset: Charset,
    ) -> Result<Box<dyn CodecHandler>> {
        let buffer = read_header_page(channel)?;
        let flags = buffer.get_u8_at(ENCRYPTION_FLAGS_OFFSET)?;
        if flags & NEW_ENCRYPTION != 0 {
            Ok(Box::new(MsisamCryptCodecHandler::new(
                password, charset, &buffer,
            )?))
        } else {
            // old MSISAM dbs use jet-style encryption w/ a different key
            let encoding_key = get_old_decryption_key(&buffer, &channel.format)?;
            Ok(Box::new(JetCryptCodecHandler::with_max_encoded_page(
                encoding_key,
                MSISAM_MAX_ENCRYPTED_PAGE,
            )))
        }
    }

    /// `MSISAMCryptCodecHandler(PageChannel, String, Charset, ByteBuffer)`:
    /// derive the key and verify the password against the page 0 check bytes.
    pub fn new(
        password: Option<&str>,
        charset: Charset,
        buffer: &ByteBuf,
    ) -> Result<MsisamCryptCodecHandler> {
        let salt = buffer.get_bytes_at(SALT_OFFSET, SALT_BYTES)?;

        // create decryption key parts
        let pwd_digest = create_password_digest(buffer, password, charset)?;
        let base_salt = salt[..SALT_LENGTH].to_vec();

        // check password hash using decryption of a known sequence
        verify_password(buffer, &pwd_digest, &salt, &base_salt)?;

        // create final key
        let mut base_hash = pwd_digest;
        base_hash.extend_from_slice(&base_salt);
        Ok(MsisamCryptCodecHandler {
            base_hash,
            param_cache: KeyCache::new(),
        })
    }

    /// `_baseHash`, exposed for the tests and for fixture checks.
    pub fn base_hash(&self) -> &[u8] {
        &self.base_hash
    }

    /// `BaseJetCryptCodecHandler.isEncryptedPage`: only pages 1..=14.
    fn is_encrypted_page(&self, page_number: i32) -> bool {
        page_number > 0 && page_number <= MSISAM_MAX_ENCRYPTED_PAGE
    }

    /// `MSISAMCryptCodecHandler.computeCipherParams`: the key for the page.
    fn page_key(&mut self, page_number: i32) -> Vec<u8> {
        let base = &self.base_hash;
        self.param_cache
            .get(page_number, || {
                apply_page_number(base, PASSWORD_DIGEST_LENGTH, page_number)
            })
            .to_vec()
    }

    /// `BaseCryptCodecHandler.streamDecrypt` / `streamEncrypt` for RC4 (RC4 is
    /// its own inverse, so one routine covers both directions).
    fn apply_keystream(&mut self, data: &mut [u8], page_number: i32) {
        let key = self.page_key(page_number);
        let mut cipher = Rc4::new_from_slice(&key).expect("20 byte keys are supported");
        cipher.apply_keystream(data);
    }
}

impl CodecHandler for MsisamCryptCodecHandler {
    fn decode_page_inline(&mut self, page: &mut ByteBuf, page_number: i32) -> Result<()> {
        if !self.is_encrypted_page(page_number) {
            // not encoded
            return Ok(());
        }
        let limit = page.limit();
        self.apply_keystream(&mut page.array_mut()[..limit], page_number);
        Ok(())
    }

    fn encode_page(
        &mut self,
        page: &ByteBuf,
        page_number: i32,
        _page_offset: usize,
    ) -> Result<ByteBuf> {
        if !self.is_encrypted_page(page_number) {
            // not encoded
            return Ok(page.dup());
        }
        // note, the stream always starts at offset 0 so that the cipher is
        // applied to the correct part of the stream, exactly like
        // `BaseCryptCodecHandler.streamEncrypt`
        let limit = page.limit();
        let mut out = page.dup();
        self.apply_keystream(&mut out.array_mut()[..limit], page_number);
        Ok(out)
    }
}

/// `MSISAMCryptCodecHandler.verifyPassword`: decrypt the 4 check bytes with
/// the password digest key and compare them with the first 4 salt bytes.  A
/// blank check means the database has no password.
fn verify_password(
    buffer: &ByteBuf,
    pwd_digest: &[u8],
    salt: &[u8],
    test_bytes: &[u8],
) -> Result<()> {
    let encrypted4_bytes_check = get_password_test_bytes(buffer)?;
    if is_blank_key(&encrypted4_bytes_check) {
        // no password?
        return Ok(());
    }

    let mut key = pwd_digest.to_vec();
    key.extend_from_slice(salt);
    let mut engine = Rc4::new_from_slice(&key).expect("24 byte keys are supported");
    let mut decrypted = encrypted4_bytes_check;
    engine.apply_keystream(&mut decrypted);

    if decrypted != test_bytes {
        return Err(Error::InvalidCredentials(INCORRECT_PASSWORD.into()));
    }
    Ok(())
}

/// `MSISAMCryptCodecHandler.createPasswordDigest`: MD5 or SHA-1 (by the
/// `USE_SHA1` flag) of the upper cased password padded to
/// [`PASSWORD_LENGTH`] bytes, truncated to [`PASSWORD_DIGEST_LENGTH`] bytes.
pub fn create_password_digest(
    buffer: &ByteBuf,
    password: Option<&str>,
    charset: Charset,
) -> Result<Vec<u8>> {
    let flags = buffer.get_u8_at(ENCRYPTION_FLAGS_OFFSET)?;
    let digest = if flags & USE_SHA1 != 0 {
        HashAlgorithm::Sha1
    } else {
        HashAlgorithm::Md5
    };

    let mut password_bytes = [0u8; PASSWORD_LENGTH];
    if let Some(password) = password {
        let encoded = encode_uncompressed_text(&password.to_uppercase(), charset);
        let count = password_bytes.len().min(encoded.len());
        password_bytes[..count].copy_from_slice(&encoded[..count]);
    }

    // Get digest value
    Ok(fix_to_length(
        &digest.hash(&password_bytes),
        PASSWORD_DIGEST_LENGTH,
    ))
}

/// `MSISAMCryptCodecHandler.getOldDecryptionKey`: the key the old (jet style)
/// MSISAM encoding uses.  It is derived from page 0 only, so no password is
/// needed.
fn get_old_decryption_key(buffer: &ByteBuf, format: &JetFormat) -> Result<Vec<u8>> {
    // `JetCryptCodecHandler.ENCODING_KEY_LENGTH` bytes from the salt offset
    let mut encoding_key =
        buffer.get_bytes_at(SALT_OFFSET, crate::crypt::jet::ENCODING_KEY_LENGTH)?;

    // Hash the salt. Step 1.
    {
        let mut full_hash_data = read_password_region(buffer, format)?;

        // msisam also unmasks a trailing portion of the region, which the
        // password field itself does not cover
        if let Some(pwd_mask) = get_password_mask(buffer, format)? {
            let trailing_offset = full_hash_data.len() - TRAILING_PWD_LEN;
            for i in 0..TRAILING_PWD_LEN {
                full_hash_data[trailing_offset + i] ^= pwd_mask[i % pwd_mask.len()];
            }
        }

        let mut hash_data = vec![0u8; format.size_password];
        for (pos, out) in hash_data.iter_mut().enumerate() {
            *out = full_hash_data[pos * 2];
        }

        hash_salt(&mut encoding_key, &hash_data);
    }

    // Hash the salt. Step 2
    {
        let jet_header = buffer.get_bytes_at(OFFSET_ENGINE_NAME, LENGTH_ENGINE_NAME)?;
        hash_salt(&mut encoding_key, &jet_header);
    }

    Ok(encoding_key)
}

/// `MSISAMCryptCodecHandler.getPasswordTestBytes`: 4 bytes at
/// [`CRYPT_CHECK_START`] plus the low byte of the salt.
fn get_password_test_bytes(buffer: &ByteBuf) -> Result<Vec<u8>> {
    let crypt_check_offset = buffer.get_u8_at(SALT_OFFSET)? as usize;
    buffer.get_bytes_at(CRYPT_CHECK_START + crypt_check_offset, 4)
}

/// `MSISAMCryptCodecHandler.hashSalt`: mixes the given data into the first
/// four bytes of `salt` (read as a little endian int), shifting each byte by
/// its position modulo 0x18.
fn hash_salt(salt: &mut [u8], hash_data: &[u8]) {
    let mut hash = i32::from_le_bytes([salt[0], salt[1], salt[2], salt[3]]);

    for (pos, &data) in hash_data.iter().enumerate() {
        let mut tmp = data as i32;
        tmp = tmp.wrapping_shl((pos % 0x18) as u32);
        hash ^= tmp;
    }

    salt[..4].copy_from_slice(&hash.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytebuf::ByteBuf;
    use std::path::{Path, PathBuf};

    /// The page 0 fields the handler reads: the salt, the flags, the password
    /// region and the 4 check bytes (at `CRYPT_CHECK_START + salt[0]`).
    /// Everything else is zero.  The page is *de-masked* (`readHeaderPage`),
    /// which is what the handler is given.
    fn header(flags: u8, salt: [u8; 8], pwd: &[u8], check: [u8; 4]) -> ByteBuf {
        let mut page = vec![0u8; 4096];
        page[ENCRYPTION_FLAGS_OFFSET] = flags;
        page[SALT_OFFSET..SALT_OFFSET + 8].copy_from_slice(&salt);
        // `JetFormat.VERSION_4.OFFSET_PASSWORD`
        page[66..66 + pwd.len()].copy_from_slice(pwd);
        let check_off = CRYPT_CHECK_START + salt[0] as usize;
        page[check_off..check_off + 4].copy_from_slice(&check);
        ByteBuf::wrap_le(page)
    }

    // The fixture digests below were produced by the *Java*
    // `MSISAMCryptCodecHandler.createPasswordDigest` itself (called by
    // reflection with the same flags, password and UTF-16LE charset), so they
    // pin the port to the reference implementation, not to a second
    // implementation of the same idea.

    /// SHA-1 digest of the upper cased UTF-16LE password padded to 0x28 bytes,
    /// truncated to 16 bytes.
    #[test]
    fn sha1_password_digest_matches_the_java_fixture() {
        let page = header(USE_SHA1 | 0x1d, [0u8; 8], &[], [0u8; 4]);
        assert_eq!(
            hex(&create_password_digest(&page, Some("Test12345"), Charset::Utf16Le).unwrap()),
            "ec6cc250ce16d2f66b5d72fd9236a62b"
        );
        // `password.toUpperCase(Locale.ROOT)` before encoding
        assert_eq!(
            hex(&create_password_digest(&page, Some("tEST12345"), Charset::Utf16Le).unwrap()),
            "ec6cc250ce16d2f66b5d72fd9236a62b"
        );
        // a missing password hashes the all zero 0x28 byte buffer
        assert_eq!(
            hex(&create_password_digest(&page, None, Charset::Utf16Le).unwrap()),
            "b80de5d138758541c5f05265ad144ab9"
        );
    }

    /// The MD5 digest (no `USE_SHA1` flag) of the same passwords.
    #[test]
    fn md5_password_digest_matches_the_java_fixture() {
        let page = header(0x01, [0u8; 8], &[], [0u8; 4]);
        assert_eq!(
            hex(&create_password_digest(&page, Some("Test12345"), Charset::Utf16Le).unwrap()),
            "e20101841abd3665fdde099ceb5241e0"
        );
        assert_eq!(
            hex(&create_password_digest(&page, None, Charset::Utf16Le).unwrap()),
            "fd4b38e94292e00251b9f39c47ee5710"
        );
    }

    /// The per page key is the 20 byte base hash (digest + first 4 salt bytes)
    /// with the page number xored in at offset 16
    /// (`PASSWORD_DIGEST_LENGTH`), and the encoding covers pages 1..=14 only.
    #[test]
    fn page_key_and_page_range_match_the_java_fixture() {
        let digest = hex_to_vec("ec6cc250ce16d2f66b5d72fd9236a62b");
        let salt = [0x73u8, 0xcb, 0x97, 0x96, 0x32, 0xc4, 0xe3, 0x40];
        let mut base = digest;
        base.extend_from_slice(&salt[..SALT_LENGTH]);
        assert_eq!(hex(&base), "ec6cc250ce16d2f66b5d72fd9236a62b73cb9796");

        // page 1 xors 1 into the salt bytes ((int)1 little endian)
        assert_eq!(
            hex(&apply_page_number(&base, PASSWORD_DIGEST_LENGTH, 1)),
            "ec6cc250ce16d2f66b5d72fd9236a62b72cb9796"
        );
        // page 14: 0x73 ^ 0x0e
        assert_eq!(
            hex(&apply_page_number(&base, PASSWORD_DIGEST_LENGTH, 14)),
            "ec6cc250ce16d2f66b5d72fd9236a62b7dcb9796"
        );

        let handler = MsisamCryptCodecHandler {
            base_hash: base,
            param_cache: KeyCache::new(),
        };
        assert!(handler.is_encrypted_page(1));
        assert!(handler.is_encrypted_page(MSISAM_MAX_ENCRYPTED_PAGE));
        // page 0 and every page above 14 are clear
        assert!(!handler.is_encrypted_page(0));
        assert!(!handler.is_encrypted_page(MSISAM_MAX_ENCRYPTED_PAGE + 1));
        assert!(!handler.is_encrypted_page(100));
    }

    /// `hashSalt` on a known salt/data pair, checked against the Java
    /// expression (`int` read little endian, `byte << (pos % 0x18)` xored in).
    #[test]
    fn hash_salt_matches_the_java_expression() {
        let mut salt = [0x01u8, 0x02, 0x03, 0x04];
        hash_salt(&mut salt, &[0x11, 0x22, 0x33]);
        // hash = 0x04030201 ^ 0x11 ^ (0x22 << 1) ^ (0x33 << 2)
        let expected: i32 = 0x04030201 ^ 0x11 ^ (0x22 << 1) ^ (0x33 << 2);
        assert_eq!(salt, expected.to_le_bytes());
    }

    /// A modern header with the password check bytes in place verifies the
    /// right password and rejects a wrong or missing one with the Java message.
    #[test]
    fn password_verifier_rejects_a_wrong_password() {
        // the real money2008-pwd.mny salt and flags
        let salt = [0x73u8, 0xcb, 0x97, 0x96, 0x32, 0xc4, 0xe3, 0x40];
        let flags = USE_SHA1 | 0x1d;
        let mut key = create_password_digest(
            &header(flags, salt, &[], [0u8; 4]),
            Some("Test12345"),
            Charset::Utf16Le,
        )
        .unwrap();
        key.extend_from_slice(&salt);
        let mut engine = Rc4::new_from_slice(&key).unwrap();
        let mut check = salt[..SALT_LENGTH].to_vec();
        engine.apply_keystream(&mut check);
        let check: [u8; 4] = check.try_into().unwrap();
        assert_eq!(hex(&check), "d9de64ed", "the real file's check bytes");

        let page = header(flags, salt, &[], check);
        let digest = create_password_digest(&page, Some("Test12345"), Charset::Utf16Le).unwrap();
        verify_password(&page, &digest, &salt, &salt[..SALT_LENGTH]).unwrap();

        let wrong = create_password_digest(&page, Some("WrongPassword"), Charset::Utf16Le).unwrap();
        let err = verify_password(&page, &wrong, &salt, &salt[..SALT_LENGTH]).unwrap_err();
        match err {
            Error::InvalidCredentials(message) => assert_eq!(message, INCORRECT_PASSWORD),
            other => panic!("expected InvalidCredentials, got {:?}", other),
        }

        // a missing password is wrong too (the check bytes are not blank)
        let none = create_password_digest(&page, None, Charset::Utf16Le).unwrap();
        assert!(matches!(
            verify_password(&page, &none, &salt, &salt[..SALT_LENGTH]),
            Err(Error::InvalidCredentials(_))
        ));
    }

    /// A blank check area means "no password": any password (or none) passes.
    #[test]
    fn blank_check_bytes_skip_the_password_check() {
        let salt = [0xab, 0x38, 0xce, 0x00, 0x5e, 0xfa, 0x83, 0x66];
        let page = header(USE_SHA1 | 0x1d, salt, &[], [0u8; 4]);
        for password in [None, Some("WrongPassword"), Some("Test12345")] {
            let digest = create_password_digest(&page, password, Charset::Utf16Le).unwrap();
            verify_password(&page, &digest, &salt, &salt[..SALT_LENGTH]).unwrap();
        }
    }

    /// `getOldDecryptionKey` on the real Money 2001 headers, checked against
    /// the value the Java `MSISAMCryptCodecHandler` derives from the same page
    /// (the password protected file has a different password field, hence a
    /// different key; neither needs the password itself).
    #[test]
    fn old_decryption_key_matches_the_java_fixture() {
        for (name, expected) in [
            ("money2001.mny", "3dbb747e"),
            ("money2001-pwd.mny", "9f9e747e"),
            ("money2002.mny", "bf422756"),
        ] {
            let Some(path) = msisam_fixture(name) else {
                continue;
            };
            let page = real_header_page(&path);
            let format = JetFormat::msisam();
            let key = get_old_decryption_key(&page, &format).unwrap();
            assert_eq!(hex(&key), expected, "old key of {}", name);
        }
    }

    /// The header fields of the five real files, as the port parses them
    /// (de-masked page 0): flag byte, salt, and the password check offset.
    #[test]
    fn real_header_fields_parse_like_the_java_handler() {
        let cases = [
            ("money2001.mny", 0x01, "50b5177e3bc4e340", "00000000"),
            ("money2002.mny", 0x05, "ee1831565ce3e340", "00000000"),
            ("money2008.mny", 0x3d, "b977849432c4e340", "c26c6f1b"),
            ("money2001-pwd.mny", 0x01, "50b5177e3bc4e340", "00000000"),
            ("money2008-pwd.mny", 0x3d, "73cb979632c4e340", "d9de64ed"),
        ];
        for (name, flags, salt, check) in cases {
            let Some(path) = msisam_fixture(name) else {
                continue;
            };
            let page = real_header_page(&path);
            assert_eq!(page.get_u8_at(ENCRYPTION_FLAGS_OFFSET).unwrap(), flags);
            assert_eq!(
                hex(&page.get_bytes_at(SALT_OFFSET, SALT_BYTES).unwrap()),
                salt,
                "salt of {}",
                name
            );
            assert_eq!(
                hex(&get_password_test_bytes(&page).unwrap()),
                check,
                "check bytes of {}",
                name
            );
        }
    }

    fn msisam_fixture(name: &str) -> Option<PathBuf> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("external")
            .join("jackcessencrypt-test")
            .join(name);
        if path.exists() {
            Some(path)
        } else {
            eprintln!(
                "skipping MSISAM fixture test: {} is missing",
                path.display()
            );
            None
        }
    }

    /// The de-masked page 0 of a real file (`PageChannel.readRootPage`).
    fn real_header_page(path: &Path) -> ByteBuf {
        let mut page = vec![0u8; 4096];
        let all = std::fs::read(path).unwrap();
        page.copy_from_slice(&all[..4096]);
        let format = JetFormat::msisam();
        for (i, mask) in format.header_mask.iter().enumerate() {
            page[format.offset_masked_header + i] ^= mask;
        }
        ByteBuf::wrap_le(page)
    }

    fn hex(bytes: &[u8]) -> String {
        let mut out = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            out.push_str(&format!("{:02x}", b));
        }
        out
    }

    fn hex_to_vec(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }
}
