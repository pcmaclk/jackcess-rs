//! Port of `com.healthmarketscience.jackcess.crypt.impl.OfficeCryptCodecHandler`
//! and the provider dispatch of `CryptCodecProvider`'s Office path.
//!
//! An encrypted `.accdb` starts with the ordinary jet header page (which is
//! never encrypted) holding the file's encoding key, and carries an
//! `EncryptionInfo` structure at [`CRYPT_STRUCTURE_OFFSET`]: a small version
//! tag followed by either the agile XML descriptor or the older
//! `EncryptionHeader` / `EncryptionVerifier` structures.  Every page above 0
//! is then encrypted as a whole page; page 0 is the file's salt.
//!
//! The handler never decodes a page in place when the cipher is a block
//! cipher, and never encodes part of a page in that case either, which is what
//! `BlockCipherProvider` says; the RC4 providers can do both.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::{is_blank_key, read_header_page, CodecHandler, DummyHandler};
use crate::crypt::office::agile::AgileEncryptionProvider;
use crate::crypt::office::header::{is_flag_set, FAES_FLAG, FCRYPTO_API_FLAG};
use crate::crypt::office::standard::{
    BinaryRc4Provider, EcmaStandardProvider, Rc4CryptoApiProvider, HASH_ITERATIONS,
    NON_STANDARD_HASH_ITERATIONS,
};
use crate::error::{Error, Result};
use crate::pagechannel::PageChannel;

/// `OfficeCryptCodecHandler.CRYPT_STRUCTURE_OFFSET`: where the `EncryptionInfo`
/// structure begins in page 0.
pub const CRYPT_STRUCTURE_OFFSET: usize = 0x299;

/// `OfficeCryptCodecHandler.MAX_PASSWORD_LEN`.
pub const MAX_PASSWORD_LEN: usize = 255;

/// `OfficeCryptCodecHandler.getPasswordBytes`: the bytes a password
/// contributes to a key.  Anything past `MAX_PASSWORD_LEN` characters is
/// dropped, so a writer which keeps them would produce a database it could not
/// reopen.  Java truncates the `String` by its UTF-16 length, which the port
/// reproduces exactly (the cut can split a surrogate pair, as in Java).
pub fn get_password_bytes(password: Option<&str>) -> Vec<u8> {
    let Some(password) = password else {
        return Vec::new();
    };
    let mut units: Vec<u16> = password.encode_utf16().collect();
    if units.len() > MAX_PASSWORD_LEN {
        units.truncate(MAX_PASSWORD_LEN);
    }
    // EncryptionHeader.UNICODE_CHARSET ("UTF-16LE")
    let mut bytes = Vec::with_capacity(units.len() * 2);
    for u in units {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    bytes
}

/// The readable scheme behind a handler.
// The providers carry the parsed encryption header/key material; boxing the
// large variant would only add an indirection to every page decrypt/encrypt.
#[expect(
    clippy::large_enum_variant,
    reason = "variants own the parsed provider state; boxing adds indirection to the page hot path"
)]
enum Variant {
    Agile(AgileEncryptionProvider),
    Standard(EcmaStandardProvider),
    Rc4CryptoApi(Rc4CryptoApiProvider),
    BinaryRc4(BinaryRc4Provider),
}

impl Variant {
    /// `BlockCipherProvider` versus `StreamCipherProvider`.
    fn is_block_cipher(&self) -> bool {
        matches!(self, Variant::Agile(_) | Variant::Standard(_))
    }

    fn verify_password(&self, pwd_bytes: &[u8]) -> Result<bool> {
        match self {
            Variant::Agile(p) => p.verify_password(pwd_bytes),
            Variant::Standard(p) => p.verify_password(),
            Variant::Rc4CryptoApi(p) => p.verify_password(),
            Variant::BinaryRc4(p) => p.verify_password(),
        }
    }

    fn decrypt_page(&self, encoding_key: &[u8], page_number: i32, page: &mut [u8]) -> Result<()> {
        match self {
            Variant::Agile(p) => p.decrypt_page(encoding_key, page_number, page),
            Variant::Standard(p) => p.decrypt_page(encoding_key, page_number, page),
            Variant::Rc4CryptoApi(p) => p.decrypt_page(encoding_key, page_number, page),
            Variant::BinaryRc4(p) => p.decrypt_page(encoding_key, page_number, page),
        }
    }

    fn encrypt_page(&self, encoding_key: &[u8], page_number: i32, page: &mut [u8]) -> Result<()> {
        match self {
            Variant::Agile(p) => p.encrypt_page(encoding_key, page_number, page),
            Variant::Standard(p) => p.encrypt_page(encoding_key, page_number, page),
            Variant::Rc4CryptoApi(p) => p.encrypt_page(encoding_key, page_number, page),
            Variant::BinaryRc4(p) => p.encrypt_page(encoding_key, page_number, page),
        }
    }
}

/// Port of `OfficeCryptCodecHandler`.
pub struct OfficeCryptCodecHandler {
    encoding_key: Vec<u8>,
    page_size: usize,
    variant: Variant,
}

/// `OfficeCryptCodecHandler.create`: reads page 0, builds the provider the
/// `EncryptionInfo` structure names, verifies the password and returns the
/// decoding handler (or the dummy handler when the file is not encrypted).
pub fn create_handler(
    channel: &mut PageChannel,
    password: Option<&str>,
) -> Result<Box<dyn CodecHandler>> {
    let buffer = read_header_page(channel)?;
    let format = channel.format.clone();
    let page_size = format.page_size;

    // a non-zero encoding key indicates the db is encoded - will be passed to
    // the handler for use in encryption
    let encoding_key = buffer.get_bytes_at(format.offset_encoding_key, 4)?;

    if is_blank_key(&encoding_key) {
        return Ok(Box::new(DummyHandler));
    }

    let info_len = buffer.get_u16_at(CRYPT_STRUCTURE_OFFSET)? as usize;
    let info_bytes = buffer.get_bytes_at(CRYPT_STRUCTURE_OFFSET + 2, info_len)?;
    let mut enc_prov_buf = ByteBuf::wrap_le(info_bytes);

    // read encoding provider version (OC 2.1.4)
    let v_major = enc_prov_buf.get_u16()?;
    let v_minor = enc_prov_buf.get_u16()?;

    let pwd_bytes = get_password_bytes(password);

    let variant = match (v_major, v_minor) {
        // OC 2.3.4.10 - Agile Encryption: 4,4
        (4, 4) => Variant::Agile(AgileEncryptionProvider::new(&mut enc_prov_buf, &pwd_bytes)?),
        // OC 2.3.6.1 - RC4 Encryption: 1,1
        (1, 1) => Variant::BinaryRc4(BinaryRc4Provider::new(&mut enc_prov_buf, &pwd_bytes)?),
        // OC 2.3.4.6 - Extensible Encryption: (3,4),3
        (3 | 4, 3) => {
            return Err(Error::Unsupported(
                "Extensible encryption provider is not supported".into(),
            ))
        }
        // OC 2.3.4.5 Standard Encryption and 2.3.5.1 RC4 CryptoAPI: (2,3,4),2
        (2..=4, 2) => {
            // read flags (copy of the flags in EncryptionHeader)
            let flags = enc_prov_buf.get_i32()?;
            if !is_flag_set(flags, FCRYPTO_API_FLAG) {
                return Err(Error::Unsupported(format!(
                    "Unsupported office encryption provider: vMajor {}, vMinor {}",
                    v_major, v_minor
                )));
            }
            if is_flag_set(flags, FAES_FLAG) {
                Variant::Standard(EcmaStandardProvider::new(
                    &mut enc_prov_buf,
                    &pwd_bytes,
                    HASH_ITERATIONS,
                )?)
            } else {
                let init_pos = enc_prov_buf.position();
                match Rc4CryptoApiProvider::new(&mut enc_prov_buf, &pwd_bytes) {
                    Ok(p) => Variant::Rc4CryptoApi(p),
                    Err(rc4_error) => {
                        // is this the "non-standard" encryption provider?
                        enc_prov_buf.set_position(init_pos)?;
                        match EcmaStandardProvider::new(
                            &mut enc_prov_buf,
                            &pwd_bytes,
                            NON_STANDARD_HASH_ITERATIONS,
                        ) {
                            Ok(p) => Variant::Standard(p),
                            Err(_) => return Err(rc4_error),
                        }
                    }
                }
            }
        }
        _ => {
            return Err(Error::Unsupported(format!(
                "Unsupported office encryption provider: vMajor {}, vMinor {}",
                v_major, v_minor
            )))
        }
    };

    if !variant.verify_password(&pwd_bytes)? {
        return Err(Error::InvalidCredentials(
            "Incorrect password provided".into(),
        ));
    }

    Ok(Box::new(OfficeCryptCodecHandler {
        encoding_key,
        page_size,
        variant,
    }))
}

impl OfficeCryptCodecHandler {
    /// `BaseJetCryptCodecHandler.isEncryptedPage`: page 0 is never encrypted.
    fn is_encrypted_page(page_number: i32) -> bool {
        page_number > 0
    }

    /// Decrypts a whole page into a fresh page sized buffer.
    fn decrypt_into(&self, page: &[u8], page_number: i32) -> Result<Vec<u8>> {
        let mut data = vec![0u8; self.page_size];
        let n = page.len().min(self.page_size);
        data[..n].copy_from_slice(&page[..n]);
        self.variant
            .decrypt_page(&self.encoding_key, page_number, &mut data)?;
        Ok(data)
    }
}

impl CodecHandler for OfficeCryptCodecHandler {
    /// `BlockCipherProvider.canEncodePartialPage` is false; an RC4 provider
    /// can encode part of a page.
    fn can_encode_partial_page(&self) -> bool {
        !self.variant.is_block_cipher()
    }

    /// `BlockCipherProvider.canDecodeInline` is false.
    fn can_decode_inline(&self) -> bool {
        !self.variant.is_block_cipher()
    }

    fn decode_page_inline(&mut self, page: &mut ByteBuf, page_number: i32) -> Result<()> {
        if !Self::is_encrypted_page(page_number) {
            // not encoded
            return Ok(());
        }
        // stream ciphers decode on top of the input buffer
        let limit = page.limit();
        self.variant.decrypt_page(
            &self.encoding_key,
            page_number,
            &mut page.array_mut()[..limit],
        )
    }

    fn decode_page(
        &mut self,
        in_page: &ByteBuf,
        out_page: &mut ByteBuf,
        page_number: i32,
    ) -> Result<()> {
        if !Self::is_encrypted_page(page_number) {
            *out_page = in_page.dup();
            return Ok(());
        }
        let data = self.decrypt_into(in_page.bytes(), page_number)?;
        *out_page = ByteBuf::wrap_le(data);
        Ok(())
    }

    fn encode_page(
        &mut self,
        page: &ByteBuf,
        page_number: i32,
        _page_offset: usize,
    ) -> Result<ByteBuf> {
        if !Self::is_encrypted_page(page_number) {
            // not encoded
            return Ok(page.dup());
        }

        // a block cipher can only encode a whole page, so the output is a
        // page sized buffer (the Java temp buffer is cleared, leaving zeroes
        // past the input limit)
        let limit = page.limit();
        let mut data = vec![0u8; self.page_size];
        let n = limit.min(self.page_size);
        data[..n].copy_from_slice(&page.bytes()[..n]);
        let encode_len = if self.variant.is_block_cipher() {
            // Java passes the whole buffer limit to the block cipher
            self.page_size.min(limit)
        } else {
            n
        };
        self.variant
            .encrypt_page(&self.encoding_key, page_number, &mut data[..encode_len])?;
        Ok(ByteBuf::wrap_le(data))
    }
}
