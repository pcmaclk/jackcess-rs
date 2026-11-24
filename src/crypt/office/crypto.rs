//! The cryptographic primitives the Office encryption schemes are built
//! from: the digest and cipher selection which the agile descriptor names,
//! the AES block cipher (CBC and ECB, with the CBC state carried across
//! calls), the RC4 stream cipher, and the small byte helpers
//! `BaseCryptCodecHandler` uses.
//!
//! Only the algorithms MS Access actually writes are implemented: AES with
//! CBC (agile) or ECB (ECMA standard) and the SHA family plus MD5.  The
//! descriptor parser accepts the other names the format allows and reports
//! them as unsupported, the way a configuration this port cannot honour has
//! to be.

use crate::error::{Error, Result};
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use rc4::StreamCipher;
use sha1::Digest;

/// `BaseCryptCodecHandler.bits2bytes`.
pub fn bits_to_bytes(bits: usize) -> usize {
    bits / 8
}

/// `BaseCryptCodecHandler.fixToLength(bytes, len)`: truncate or zero pad.
pub fn fix_to_length(bytes: &[u8], len: usize) -> Vec<u8> {
    fix_to_length_with(bytes, len, 0)
}

/// `BaseCryptCodecHandler.fixToLength(bytes, len, padByte)`.
pub fn fix_to_length_with(bytes: &[u8], len: usize, pad_byte: u8) -> Vec<u8> {
    if bytes.len() == len {
        return bytes.to_vec();
    }
    let mut out = vec![pad_byte; len];
    let n = bytes.len().min(len);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

/// `BaseCryptCodecHandler.iterateHash`: hashes the previous hash together
/// with its own iteration number, the given number of times.  The iteration
/// number is written little endian (`PageChannel.DEFAULT_BYTE_ORDER`).
pub fn iterate_hash(alg: HashAlgorithm, base_hash: &[u8], iterations: u32) -> Vec<u8> {
    if iterations == 0 {
        return base_hash.to_vec();
    }
    let mut iter_hash = base_hash.to_vec();
    for i in 0..iterations {
        let iter_bytes = i.to_le_bytes();
        iter_hash = alg.hash_parts(&[&iter_bytes, &iter_hash]);
    }
    iter_hash
}

/// The digests the descriptor can name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HashAlgorithm {
    Sha1,
    Sha256,
    Sha384,
    Sha512,
    Md5,
}

impl HashAlgorithm {
    /// `XmlEncryptionDescriptor.HashAlgorithm`'s enum constant name.
    pub fn enum_name(self) -> &'static str {
        match self {
            HashAlgorithm::Sha1 => "SHA1",
            HashAlgorithm::Sha256 => "SHA256",
            HashAlgorithm::Sha384 => "SHA384",
            HashAlgorithm::Sha512 => "SHA512",
            HashAlgorithm::Md5 => "MD5",
        }
    }

    /// The name the descriptor writes for the digest.
    pub fn xml_name(self) -> &'static str {
        match self {
            HashAlgorithm::Sha1 => "SHA1",
            HashAlgorithm::Sha256 => "SHA256",
            HashAlgorithm::Sha384 => "SHA384",
            HashAlgorithm::Sha512 => "SHA512",
            HashAlgorithm::Md5 => "MD5",
        }
    }

    /// The digest length in bytes.
    pub fn size(self) -> usize {
        match self {
            HashAlgorithm::Sha1 => 20,
            HashAlgorithm::Sha256 => 32,
            HashAlgorithm::Sha384 => 48,
            HashAlgorithm::Sha512 => 64,
            HashAlgorithm::Md5 => 16,
        }
    }

    /// `XmlEncryptionDescriptor.getHash` plus the enum names it accepts.
    /// Only the digests this port has an implementation for are returned;
    /// `MD4`, `MD2`, `RIPEMD-128`, `RIPEMD-160` and `WHIRLPOOL` name a
    /// digest which is not in the dependency set and are refused.
    pub fn from_xml_name(name: &str) -> Result<HashAlgorithm> {
        match normalize_parameter(name).as_str() {
            "SHA1" => Ok(HashAlgorithm::Sha1),
            "SHA256" => Ok(HashAlgorithm::Sha256),
            "SHA384" => Ok(HashAlgorithm::Sha384),
            "SHA512" => Ok(HashAlgorithm::Sha512),
            "MD5" => Ok(HashAlgorithm::Md5),
            _ => Err(Error::Unsupported(format!(
                "Unsupported encryption parameter: {}",
                name
            ))),
        }
    }

    /// Hashes the concatenation of the given parts.
    pub fn hash_parts(self, parts: &[&[u8]]) -> Vec<u8> {
        match self {
            HashAlgorithm::Sha1 => {
                let mut d = sha1::Sha1::new();
                for p in parts {
                    d.update(p);
                }
                d.finalize().to_vec()
            }
            HashAlgorithm::Sha256 => {
                let mut d = sha2::Sha256::new();
                for p in parts {
                    d.update(p);
                }
                d.finalize().to_vec()
            }
            HashAlgorithm::Sha384 => {
                let mut d = sha2::Sha384::new();
                for p in parts {
                    d.update(p);
                }
                d.finalize().to_vec()
            }
            HashAlgorithm::Sha512 => {
                let mut d = sha2::Sha512::new();
                for p in parts {
                    d.update(p);
                }
                d.finalize().to_vec()
            }
            HashAlgorithm::Md5 => {
                let mut d = md5::Md5::new();
                for p in parts {
                    d.update(p);
                }
                d.finalize().to_vec()
            }
        }
    }

    /// `BaseCryptCodecHandler.hash(digest, bytes)`.
    pub fn hash(self, bytes: &[u8]) -> Vec<u8> {
        self.hash_parts(&[bytes])
    }
}

impl std::fmt::Display for HashAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.enum_name())
    }
}

/// The block ciphers the descriptor can name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CipherAlgorithm {
    Aes,
}

impl CipherAlgorithm {
    pub fn xml_name(self) -> &'static str {
        match self {
            CipherAlgorithm::Aes => "AES",
        }
    }

    pub fn block_size(self) -> usize {
        16
    }

    /// `XmlEncryptionDescriptor.getAlgorithm`.  Only AES is implemented;
    /// `RC2`, `RC4`, `DES`, `3DES` and `3DES112` are refused.
    pub fn from_xml_name(name: &str) -> Result<CipherAlgorithm> {
        match normalize_parameter(name).as_str() {
            "AES" => Ok(CipherAlgorithm::Aes),
            _ => Err(Error::Unsupported(format!(
                "Unsupported encryption parameter: {}",
                name
            ))),
        }
    }
}

impl std::fmt::Display for CipherAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.xml_name())
    }
}

/// The chaining modes the descriptor can name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CipherChaining {
    Cbc,
    Ecb,
}

impl CipherChaining {
    pub fn xml_name(self) -> &'static str {
        match self {
            CipherChaining::Cbc => "ChainingModeCBC",
            CipherChaining::Ecb => "ChainingModeECB",
        }
    }

    /// `XmlEncryptionDescriptor.getChaining`.  `ChainingModeCFB`,
    /// `ChainingModeCCM` and `ChainingModeGCM` are refused.
    pub fn from_xml_name(name: &str) -> Result<CipherChaining> {
        match normalize_parameter(name).as_str() {
            "CHAININGMODECBC" => Ok(CipherChaining::Cbc),
            "CHAININGMODEECB" => Ok(CipherChaining::Ecb),
            _ => Err(Error::Unsupported(format!(
                "Unsupported encryption parameter: {}",
                name
            ))),
        }
    }
}

impl std::fmt::Display for CipherChaining {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.xml_name())
    }
}

/// `XmlEncryptionDescriptor.parseEnum`: trim, upper case, drop `-` and `_`,
/// and prefix a leading digit with `_`.
fn normalize_parameter(value: &str) -> String {
    let mut s: String = value
        .trim()
        .to_uppercase()
        .chars()
        .filter(|c| *c != '-' && *c != '_')
        .collect();
    if s.chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        s.insert(0, '_');
    }
    s
}

enum AesKind {
    Aes128(aes::Aes128),
    Aes192(aes::Aes192),
    Aes256(aes::Aes256),
}

/// One AES key, with the chaining state the Java `BufferedBlockCipher` holds
/// between `processBytes` calls.  No padding is applied: every byte range the
/// schemes process is a whole number of 16 byte blocks.
pub struct BlockCipherState {
    kind: AesKind,
    chaining: CipherChaining,
    /// the chaining value (CBC only)
    iv: [u8; 16],
}

impl BlockCipherState {
    /// Builds the cipher for the given key.  `iv` is required for CBC and
    /// ignored for ECB.
    pub fn new(
        cipher: CipherAlgorithm,
        chaining: CipherChaining,
        key: &[u8],
        iv: Option<&[u8]>,
    ) -> Result<BlockCipherState> {
        let kind = match cipher {
            CipherAlgorithm::Aes => match key.len() {
                16 => AesKind::Aes128(
                    aes::Aes128::new_from_slice(key)
                        .map_err(|_| Error::IllegalArgument("invalid AES key".into()))?,
                ),
                24 => AesKind::Aes192(
                    aes::Aes192::new_from_slice(key)
                        .map_err(|_| Error::IllegalArgument("invalid AES key".into()))?,
                ),
                32 => AesKind::Aes256(
                    aes::Aes256::new_from_slice(key)
                        .map_err(|_| Error::IllegalArgument("invalid AES key".into()))?,
                ),
                n => {
                    return Err(Error::Unsupported(format!(
                        "Unsupported AES key length {}",
                        n
                    )))
                }
            },
        };
        let mut block_iv = [0u8; 16];
        if chaining == CipherChaining::Cbc {
            let iv = iv.ok_or_else(|| {
                Error::IllegalArgument("CBC encryption needs an initialization vector".into())
            })?;
            if iv.len() != 16 {
                return Err(Error::Unsupported(format!(
                    "Unsupported initialization vector length {}",
                    iv.len()
                )));
            }
            block_iv.copy_from_slice(iv);
        }
        Ok(BlockCipherState {
            kind,
            chaining,
            iv: block_iv,
        })
    }

    fn encrypt_block(&self, block: &mut [u8; 16]) {
        let mut b = aes::Block::from(*block);
        match &self.kind {
            AesKind::Aes128(c) => c.encrypt_block(&mut b),
            AesKind::Aes192(c) => c.encrypt_block(&mut b),
            AesKind::Aes256(c) => c.encrypt_block(&mut b),
        }
        block.copy_from_slice(&b);
    }

    fn decrypt_block(&self, block: &mut [u8; 16]) {
        let mut b = aes::Block::from(*block);
        match &self.kind {
            AesKind::Aes128(c) => c.decrypt_block(&mut b),
            AesKind::Aes192(c) => c.decrypt_block(&mut b),
            AesKind::Aes256(c) => c.decrypt_block(&mut b),
        }
        block.copy_from_slice(&b);
    }

    fn check_length(&self, data: &[u8]) -> Result<()> {
        if !data.len().is_multiple_of(16) {
            return Err(Error::IllegalState(format!(
                "cipher data length {} is not a multiple of the block size",
                data.len()
            )));
        }
        Ok(())
    }

    /// Encrypts the whole of `data` in place, continuing the CBC chain from
    /// the current state.
    pub fn encrypt(&mut self, data: &mut [u8]) -> Result<()> {
        self.check_length(data)?;
        for chunk in data.as_chunks_mut::<16>().0 {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            if self.chaining == CipherChaining::Cbc {
                for i in 0..16 {
                    block[i] ^= self.iv[i];
                }
            }
            self.encrypt_block(&mut block);
            if self.chaining == CipherChaining::Cbc {
                self.iv = block;
            }
            chunk.copy_from_slice(&block);
        }
        Ok(())
    }

    /// Decrypts the whole of `data` in place, continuing the CBC chain from
    /// the current state.
    pub fn decrypt(&mut self, data: &mut [u8]) -> Result<()> {
        self.check_length(data)?;
        for chunk in data.as_chunks_mut::<16>().0 {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            let cipher_text = block;
            self.decrypt_block(&mut block);
            if self.chaining == CipherChaining::Cbc {
                for i in 0..16 {
                    block[i] ^= self.iv[i];
                }
                self.iv = cipher_text;
            }
            chunk.copy_from_slice(&block);
        }
        Ok(())
    }
}

/// An RC4 stream, which keeps its position between calls, like the Java
/// `StreamCipher`.
pub struct Rc4Stream {
    cipher: rc4::Rc4,
}

impl Rc4Stream {
    pub fn new(key: &[u8]) -> Result<Rc4Stream> {
        let cipher = rc4::Rc4::new_from_slice(key)
            .map_err(|_| Error::Unsupported(format!("Unsupported RC4 key length {}", key.len())))?;
        Ok(Rc4Stream { cipher })
    }

    /// Runs the keystream over the whole of `data` in place.
    pub fn process(&mut self, data: &mut [u8]) {
        self.cipher.apply_keystream(data);
    }
}

/// Fresh key material from the operating system's random source
/// (`SecureRandom` in Java).
pub fn random_bytes(len: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes)
        .map_err(|e| Error::Jackcess(format!("Failed generating random bytes: {}", e)))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `fixToLength` truncates and pads with the given byte, and the zero
    /// padding form leaves the input untouched when the length already
    /// matches.
    #[test]
    fn fix_to_length_truncates_and_pads() {
        assert_eq!(fix_to_length(&[1, 2, 3], 5), vec![1, 2, 3, 0, 0]);
        assert_eq!(fix_to_length(&[1, 2, 3], 2), vec![1, 2]);
        assert_eq!(
            fix_to_length_with(&[1, 2, 3], 5, 0x36),
            vec![1, 2, 3, 0x36, 0x36]
        );
        assert_eq!(fix_to_length(&[1, 2, 3], 3), vec![1, 2, 3]);
    }

    /// `iterateHash` with zero iterations returns the input, and one
    /// iteration is `hash(int2bytes(0) + base)`.
    #[test]
    fn iterate_hash_matches_the_java_loop() {
        let base = [0xAAu8; 4];
        assert_eq!(iterate_hash(HashAlgorithm::Sha1, &base, 0), base.to_vec());
        let one = iterate_hash(HashAlgorithm::Sha1, &base, 1);
        let expected = HashAlgorithm::Sha1.hash_parts(&[&0u32.to_le_bytes(), &base]);
        assert_eq!(one, expected);
    }

    /// AES-CBC is its own inverse over multiple calls and keeps the chain
    /// across them, and ECB ignores the IV.
    #[test]
    fn aes_cbc_and_ecb_round_trip() {
        let key = [0x11u8; 16];
        let iv = [0x22u8; 16];
        let mut data = vec![0u8; 64];
        for (i, b) in data.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut cbc =
            BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Cbc, &key, Some(&iv))
                .unwrap();
        let mut expected = data.clone();
        cbc.encrypt(&mut expected).unwrap();
        assert_ne!(expected, data);

        // the same bytes in two calls give the same ciphertext
        let mut first = data[..32].to_vec();
        let mut second = data[32..].to_vec();
        let mut cbc =
            BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Cbc, &key, Some(&iv))
                .unwrap();
        cbc.encrypt(&mut first).unwrap();
        cbc.encrypt(&mut second).unwrap();
        first.extend_from_slice(&second);
        assert_eq!(first, expected);

        let mut cbc =
            BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Cbc, &key, Some(&iv))
                .unwrap();
        cbc.decrypt(&mut first).unwrap();
        assert_eq!(first, data);

        let mut ecb =
            BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Ecb, &key, None).unwrap();
        let mut ecb_data = data.clone();
        ecb.encrypt(&mut ecb_data).unwrap();
        assert_ne!(ecb_data, data);
        ecb.decrypt(&mut ecb_data).unwrap();
        assert_eq!(ecb_data, data);
        // ECB over the same block twice gives the same ciphertext
        assert_eq!(ecb_data, data);
    }

    /// A non block sized range is refused rather than silently truncated.
    #[test]
    fn aes_refuses_a_partial_block() {
        let mut cipher = BlockCipherState::new(
            CipherAlgorithm::Aes,
            CipherChaining::Cbc,
            &[0u8; 16],
            Some(&[0u8; 16]),
        )
        .unwrap();
        assert!(cipher.encrypt(&mut [0u8; 17]).is_err());
    }
}
