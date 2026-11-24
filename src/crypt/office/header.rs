//! Port of `com.healthmarketscience.jackcess.crypt.impl.office.EncryptionHeader`
//! and `EncryptionVerifier`: the OC 2.3.2 / 2.3.3 structures which the ECMA
//! standard and RC4 CryptoAPI providers carry after their version.

use crate::bytebuf::ByteBuf;
use crate::error::{Error, Result};

/// `EncryptionHeader.UNICODE_CHARSET`.
pub const UNICODE_CHARSET: &str = "UTF-16LE";

/// `EncryptionHeader.FCRYPTO_API_FLAG`.
pub const FCRYPTO_API_FLAG: i32 = 0x04;
/// `EncryptionHeader.FDOC_PROPS_FLAG`.
pub const FDOC_PROPS_FLAG: i32 = 0x08;
/// `EncryptionHeader.FEXTERNAL_FLAG`.
pub const FEXTERNAL_FLAG: i32 = 0x10;
/// `EncryptionHeader.FAES_FLAG`.
pub const FAES_FLAG: i32 = 0x20;

/// `EncryptionHeader.isFlagSet`.
pub fn is_flag_set(flags: i32, mask: i32) -> bool {
    (flags & mask) != 0
}

/// `EncryptionHeader.CryptoAlgorithm`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CryptoAlgorithm {
    External,
    Rc4,
    Aes128,
    Aes192,
    Aes256,
}

impl CryptoAlgorithm {
    /// `CryptoAlgorithm.getAlgId`.
    pub fn alg_id(self) -> i32 {
        match self {
            CryptoAlgorithm::External => 0,
            CryptoAlgorithm::Rc4 => 0x6801,
            CryptoAlgorithm::Aes128 => 0x660E,
            CryptoAlgorithm::Aes192 => 0x660F,
            CryptoAlgorithm::Aes256 => 0x6610,
        }
    }

    /// `CryptoAlgorithm.getEncryptedVerifierHashLen`: the length of the
    /// encrypted verifier hash field in the `EncryptionVerifier`.
    pub fn encrypted_verifier_hash_len(self) -> usize {
        match self {
            CryptoAlgorithm::External => 0,
            CryptoAlgorithm::Rc4 => 20,
            CryptoAlgorithm::Aes128 | CryptoAlgorithm::Aes192 | CryptoAlgorithm::Aes256 => 32,
        }
    }

    /// `CryptoAlgorithm.getKeySizeMin`.
    pub fn key_size_min(self) -> i32 {
        match self {
            CryptoAlgorithm::External => 0,
            CryptoAlgorithm::Rc4 => 0x28,
            CryptoAlgorithm::Aes128 => 0x80,
            CryptoAlgorithm::Aes192 => 0xC0,
            CryptoAlgorithm::Aes256 => 0x100,
        }
    }

    /// `CryptoAlgorithm.getKeySizeMax`.
    pub fn key_size_max(self) -> i32 {
        match self {
            CryptoAlgorithm::External => 0,
            CryptoAlgorithm::Rc4 => 0x200,
            CryptoAlgorithm::Aes128 => 0x80,
            CryptoAlgorithm::Aes192 => 0xC0,
            CryptoAlgorithm::Aes256 => 0x100,
        }
    }

    /// `CryptoAlgorithm.isValidKeySize`.
    pub fn is_valid_key_size(self, key_size: i32) -> bool {
        self.key_size_min() <= key_size && key_size <= self.key_size_max()
    }

    /// The enum constant name, for error messages.
    pub fn name(self) -> &'static str {
        match self {
            CryptoAlgorithm::External => "EXTERNAL",
            CryptoAlgorithm::Rc4 => "RC4",
            CryptoAlgorithm::Aes128 => "AES_128",
            CryptoAlgorithm::Aes192 => "AES_192",
            CryptoAlgorithm::Aes256 => "AES_256",
        }
    }
}

impl std::fmt::Display for CryptoAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// `EncryptionHeader.HashAlgorithm`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HeaderHashAlgorithm {
    External,
    Sha1,
}

impl HeaderHashAlgorithm {
    pub fn alg_id(self) -> i32 {
        match self {
            HeaderHashAlgorithm::External => 0,
            HeaderHashAlgorithm::Sha1 => 0x8004,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            HeaderHashAlgorithm::External => "EXTERNAL",
            HeaderHashAlgorithm::Sha1 => "SHA1",
        }
    }
}

impl std::fmt::Display for HeaderHashAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Port of `EncryptionHeader`.
#[derive(Clone, Debug)]
pub struct EncryptionHeader {
    pub flags: i32,
    pub size_extra: i32,
    pub crypto_algorithm: CryptoAlgorithm,
    pub hash_algorithm: HeaderHashAlgorithm,
    pub key_size: i32,
    pub provider_type: i32,
    pub csp_name: String,
}

/// `ALGID_FLAGS`, `ALGID_RC4`, ... and the hash ids.
const ALGID_FLAGS: i32 = 0;
const ALGID_RC4: i32 = 0x6801;
const ALGID_AES_128: i32 = 0x660E;
const ALGID_AES_192: i32 = 0x660F;
const ALGID_AES_256: i32 = 0x6610;
const HASHALGID_FLAGS: i32 = 0;
const HASHALGID_SHA1: i32 = 0x8004;

const CSP_BASE_STRING: &str = " base ";
const RC4_BASE_DEFAULT_KEY_SIZE: i32 = 0x28;
const RC4_STRONG_DEFAULT_KEY_SIZE: i32 = 0x80;

impl EncryptionHeader {
    /// `EncryptionHeader.read`: reads the header length, then the header
    /// narrowed to that length, validates the algorithms and the key size,
    /// and leaves the buffer positioned after the header.
    pub fn read(
        buffer: &mut ByteBuf,
        valid_crypto_algos: &[CryptoAlgorithm],
        valid_hash_algos: &[HeaderHashAlgorithm],
    ) -> Result<EncryptionHeader> {
        let header_len = buffer.get_u32()? as usize;
        let start_pos = buffer.position();
        let end_pos = start_pos + header_len;
        if end_pos > buffer.limit() {
            return Err(Error::Jackcess(format!(
                "Encryption header length {} exceeds the encryption info",
                header_len
            )));
        }

        let header = {
            let mut view = buffer.narrow(start_pos, end_pos)?;
            EncryptionHeader::parse(&mut view)?
        };

        // verify parameters
        if !valid_crypto_algos.contains(&header.crypto_algorithm) {
            return Err(Error::Unsupported(format!(
                "{} crypto algorithm must be one of {:?}",
                header, valid_crypto_algos
            )));
        }
        if !valid_hash_algos.contains(&header.hash_algorithm) {
            return Err(Error::Unsupported(format!(
                "{} hash algorithm must be one of {:?}",
                header, valid_hash_algos
            )));
        }
        let key_size = header.key_size;
        if !header.crypto_algorithm.is_valid_key_size(key_size) {
            return Err(Error::Unsupported(format!(
                "{} key size is outside allowable range",
                header
            )));
        }
        if key_size % 8 != 0 {
            return Err(Error::Unsupported(format!(
                "{} key size must be multiple of 8",
                header
            )));
        }

        buffer.set_position(end_pos)?;
        Ok(header)
    }

    /// `new EncryptionHeader(buffer)`.
    fn parse(buffer: &mut ByteBuf) -> Result<EncryptionHeader> {
        let flags = buffer.get_i32()?;
        let size_extra = buffer.get_i32()?;
        let alg_id = buffer.get_i32()?;
        let alg_id_hash = buffer.get_i32()?;
        let key_size = buffer.get_i32()?;
        let provider_type = buffer.get_i32()?;

        let crypto_algorithm = parse_crypto_algorithm(alg_id, flags)?;
        let hash_algorithm = parse_hash_algorithm(alg_id_hash, flags)?;

        // reserved
        buffer.get_i32()?;
        buffer.get_i32()?;

        let csp_name = read_csp_name(buffer)?;
        let key_size = parse_key_size(key_size, crypto_algorithm, &csp_name);

        Ok(EncryptionHeader {
            flags,
            size_extra,
            crypto_algorithm,
            hash_algorithm,
            key_size,
            provider_type,
            csp_name,
        })
    }
}

impl std::fmt::Display for EncryptionHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "EncryptionHeader[flags={}, alg={}, hash={}, keySize={}, csp={}]",
            self.flags, self.crypto_algorithm, self.hash_algorithm, self.key_size, self.csp_name
        )
    }
}

/// `EncryptionHeader.parseCryptoAlgorithm`.
fn parse_crypto_algorithm(alg_id: i32, flags: i32) -> Result<CryptoAlgorithm> {
    match alg_id {
        ALGID_FLAGS => {
            if is_flag_set(flags, FEXTERNAL_FLAG) {
                return Ok(CryptoAlgorithm::External);
            }
            if is_flag_set(flags, FCRYPTO_API_FLAG) {
                return Ok(if is_flag_set(flags, FAES_FLAG) {
                    CryptoAlgorithm::Aes128
                } else {
                    CryptoAlgorithm::Rc4
                });
            }
        }
        ALGID_RC4 => return Ok(CryptoAlgorithm::Rc4),
        ALGID_AES_128 => return Ok(CryptoAlgorithm::Aes128),
        ALGID_AES_192 => return Ok(CryptoAlgorithm::Aes192),
        ALGID_AES_256 => return Ok(CryptoAlgorithm::Aes256),
        _ => {}
    }
    Err(Error::Unsupported(format!(
        "Unsupported encryption algorithm {} (flags {})",
        alg_id, flags
    )))
}

/// `EncryptionHeader.parseHashAlgorithm`.
fn parse_hash_algorithm(alg_id_hash: i32, flags: i32) -> Result<HeaderHashAlgorithm> {
    match alg_id_hash {
        HASHALGID_FLAGS => {
            if is_flag_set(flags, FEXTERNAL_FLAG) {
                return Ok(HeaderHashAlgorithm::External);
            }
            Ok(HeaderHashAlgorithm::Sha1)
        }
        HASHALGID_SHA1 => Ok(HeaderHashAlgorithm::Sha1),
        _ => Err(Error::Unsupported(format!(
            "Unsupported hash algorithm {} (flags {})",
            alg_id_hash, flags
        ))),
    }
}

/// `EncryptionHeader.parseKeySize`.
fn parse_key_size(key_size: i32, crypto_alg: CryptoAlgorithm, csp_name: &str) -> i32 {
    if key_size != 0 {
        return key_size;
    }

    // if keySize is 0, then use algorithm/provider default
    if crypto_alg == CryptoAlgorithm::Rc4 {
        let csp_name = csp_name.trim().to_lowercase();
        return if csp_name.is_empty() || csp_name.contains(CSP_BASE_STRING) {
            RC4_BASE_DEFAULT_KEY_SIZE
        } else {
            RC4_STRONG_DEFAULT_KEY_SIZE
        };
    }

    crypto_alg.key_size_min()
}

/// `EncryptionHeader.readCspName`: a null terminated UTF-16LE string which
/// fills the rest of the header.
fn read_csp_name(buffer: &mut ByteBuf) -> Result<String> {
    // unicode string, must be multiple of 2
    let rem = (buffer.remaining() / 2) * 2;
    if rem == 0 {
        return Ok(String::new());
    }
    let bytes = buffer.get_bytes(rem)?;
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    Ok(String::from_utf16_lossy(&units[..end]))
}

/// Port of `EncryptionVerifier`.
#[derive(Clone, Debug)]
pub struct EncryptionVerifier {
    pub salt_size: i32,
    pub salt: Vec<u8>,
    pub encrypted_verifier: Vec<u8>,
    pub verifier_hash_size: i32,
    pub encrypted_verifier_hash: Vec<u8>,
}

impl EncryptionVerifier {
    const SALT_SIZE: i32 = 16;
    const ENC_VERIFIER_SIZE: usize = 16;

    /// `new EncryptionVerifier(buffer, cryptoAlg)`.
    pub fn read(buffer: &mut ByteBuf, crypto_alg: CryptoAlgorithm) -> Result<EncryptionVerifier> {
        let salt_size = buffer.get_i32()?;
        if salt_size != Self::SALT_SIZE {
            return Err(Error::Unsupported(format!(
                "salt size {} must be {}",
                salt_size,
                Self::SALT_SIZE
            )));
        }
        let salt = buffer.get_bytes(salt_size as usize)?;
        let encrypted_verifier = buffer.get_bytes(Self::ENC_VERIFIER_SIZE)?;
        let verifier_hash_size = buffer.get_i32()?;
        let encrypted_verifier_hash = buffer.get_bytes(crypto_alg.encrypted_verifier_hash_len())?;
        Ok(EncryptionVerifier {
            salt_size,
            salt,
            encrypted_verifier,
            verifier_hash_size,
            encrypted_verifier_hash,
        })
    }
}
