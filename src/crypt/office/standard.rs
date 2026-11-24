//! Ports of the older Office encryption providers:
//!
//! * `ECMAStandardEncryptionProvider` (OC 2.3.4.5, the Office 2007
//!   "standard" encryption: AES in ECB mode with the CryptoAPI key
//!   derivation),
//! * `NonStandardEncryptionProvider` (the same scheme with zero hash
//!   iterations, the "compatmode 0" case),
//! * `RC4CryptoAPIProvider` (OC 2.3.5.1),
//! * `OfficeBinaryDocRC4Provider` (OC 2.3.6.1).
//!
//! All four are read-only in this port: ms access writes agile for a current
//! `.accdb`, and jackcess-encrypt has no writer for the older schemes either.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::apply_page_number;
use crate::crypt::office::crypto::{
    bits_to_bytes, fix_to_length, iterate_hash, BlockCipherState, CipherAlgorithm, CipherChaining,
    HashAlgorithm, Rc4Stream,
};
use crate::crypt::office::header::{
    CryptoAlgorithm, EncryptionHeader, EncryptionVerifier, HeaderHashAlgorithm,
};
use crate::error::{Error, Result};

/// `ECMAStandardEncryptionProvider.HASH_ITERATIONS`.
pub const HASH_ITERATIONS: u32 = 50_000;
/// `NonStandardEncryptionProvider.HASH_ITERATIONS`.
pub const NON_STANDARD_HASH_ITERATIONS: u32 = 0;

/// `ECMAStandardEncryptionProvider` / `NonStandardEncryptionProvider`: AES in
/// ECB mode, with the key derived from the password salt and the block bytes.
pub struct EcmaStandardProvider {
    header: EncryptionHeader,
    verifier: EncryptionVerifier,
    iter_hash: Vec<u8>,
    enc_key_byte_size: usize,
}

impl EcmaStandardProvider {
    /// `new ECMAStandardEncryptionProvider(channel, encodingKey, encProvBuf,
    /// pwdBytes[, hashIterations])`.
    pub fn new(
        enc_prov_buf: &mut ByteBuf,
        pwd_bytes: &[u8],
        hash_iterations: u32,
    ) -> Result<EcmaStandardProvider> {
        let header = EncryptionHeader::read(
            enc_prov_buf,
            &[
                CryptoAlgorithm::Aes128,
                CryptoAlgorithm::Aes192,
                CryptoAlgorithm::Aes256,
            ],
            &[HeaderHashAlgorithm::Sha1],
        )?;
        let verifier = EncryptionVerifier::read(enc_prov_buf, header.crypto_algorithm)?;

        // OC 2.3.4.7 (part 1)
        let base_hash = ecma_standard_base_hash(pwd_bytes, &verifier.salt);
        // the Java code recomputes this for every page; it only depends on
        // the password, so the port keeps it
        let iter_hash = ecma_standard_iter_hash(&base_hash, hash_iterations);
        let enc_key_byte_size = bits_to_bytes(header.key_size as usize);

        Ok(EcmaStandardProvider {
            header,
            verifier,
            iter_hash,
            enc_key_byte_size,
        })
    }

    /// `ECMAStandardEncryptionProvider.verifyPassword`: the verifier and the
    /// verifier hash are the two halves of one ECB stream under the block 0
    /// key.
    pub fn verify_password(&self) -> Result<bool> {
        let key = self.compute_encryption_key(&[0u8; 4]);
        let mut cipher =
            BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Ecb, &key, None)?;

        let mut verifier = self.verifier.encrypted_verifier.clone();
        cipher.decrypt(&mut verifier)?;
        let mut verifier_hash = self.verifier.encrypted_verifier_hash.clone();
        cipher.decrypt(&mut verifier_hash)?;

        let verifier_hash =
            fix_to_length(&verifier_hash, self.verifier.verifier_hash_size as usize);
        let test_hash = fix_to_length(
            &HashAlgorithm::Sha1.hash(&verifier),
            self.verifier.verifier_hash_size as usize,
        );
        Ok(verifier_hash == test_hash)
    }

    /// `computeEncryptionKey(blockBytes)`.
    fn compute_encryption_key(&self, block_bytes: &[u8]) -> Vec<u8> {
        ecma_standard_derive_key(&self.iter_hash, block_bytes, self.enc_key_byte_size)
    }

    /// Decrypts one whole page in place.
    pub fn decrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let block_bytes = apply_page_number(encoding_key, 0, page_number);
        let key = self.compute_encryption_key(&block_bytes);
        let mut cipher =
            BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Ecb, &key, None)?;
        cipher.decrypt(page)
    }

    /// Encrypts one whole page in place.
    pub fn encrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let block_bytes = apply_page_number(encoding_key, 0, page_number);
        let key = self.compute_encryption_key(&block_bytes);
        let mut cipher =
            BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Ecb, &key, None)?;
        cipher.encrypt(page)
    }

    pub fn header(&self) -> &EncryptionHeader {
        &self.header
    }
}

/// `ECMAStandardEncryptionProvider.genXBytes`.
fn gen_x_bytes(final_hash: &[u8], code: u8) -> Vec<u8> {
    let mut x = vec![code; 64];
    for (i, b) in final_hash.iter().enumerate() {
        x[i] ^= *b;
    }
    x
}

// ---------------------------------------------------------------------------
// the key derivations, split out so they can be pinned against the Java
// fixtures
// ---------------------------------------------------------------------------

/// OC 2.3.4.7 (part 1) / 2.3.5.2 (part 1): `SHA1(salt || password)`.
pub fn ecma_standard_base_hash(pwd_bytes: &[u8], salt: &[u8]) -> Vec<u8> {
    HashAlgorithm::Sha1.hash_parts(&[salt, pwd_bytes])
}

/// `BaseCryptCodecHandler.iterateHash` with SHA-1, the spin count work.
pub fn ecma_standard_iter_hash(base_hash: &[u8], hash_iterations: u32) -> Vec<u8> {
    iterate_hash(HashAlgorithm::Sha1, base_hash, hash_iterations)
}

/// `ECMAStandardEncryptionProvider.cryptDeriveKey`: the hash of the iterated
/// base hash with the block bytes, folded through the two 64 byte constants
/// and truncated to the key size.
pub fn ecma_standard_derive_key(
    iter_hash: &[u8],
    block_bytes: &[u8],
    key_byte_size: usize,
) -> Vec<u8> {
    let final_hash = HashAlgorithm::Sha1.hash_parts(&[iter_hash, block_bytes]);
    let x1 = HashAlgorithm::Sha1.hash(&gen_x_bytes(&final_hash, 0x36));
    let x2 = HashAlgorithm::Sha1.hash(&gen_x_bytes(&final_hash, 0x5C));
    let mut bytes = x1;
    bytes.extend_from_slice(&x2);
    fix_to_length(&bytes, key_byte_size)
}

/// `RC4CryptoAPIProvider.computeEncryptionKey`: `SHA1(baseHash || block)`
/// truncated to the key size, padded to 16 bytes for the 40 bit case.
pub fn rc4_cryptoapi_derive_key(
    base_hash: &[u8],
    block_bytes: &[u8],
    key_byte_size: usize,
    key_size: i32,
) -> Vec<u8> {
    let hash = HashAlgorithm::Sha1.hash_parts(&[base_hash, block_bytes]);
    let mut enc_key = fix_to_length(&hash, key_byte_size);
    if key_size == 40 {
        enc_key = fix_to_length(&enc_key, bits_to_bytes(128));
    }
    enc_key
}

/// `OfficeBinaryDocRC4Provider`'s `fillHash`: the first five bytes of
/// `MD5(password)` followed by the salt.
pub fn binary_rc4_fill_hash(pwd_bytes: &[u8], salt: &[u8]) -> Vec<u8> {
    let mut v = fix_to_length(&HashAlgorithm::Md5.hash(pwd_bytes), 5);
    v.extend_from_slice(salt);
    v
}

/// OC 2.3.6.2 (part 1): the 336 byte buffer the fill hash repeats through,
/// hashed with MD5 and truncated to five bytes.
pub fn binary_rc4_base_hash(pwd_bytes: &[u8], salt: &[u8]) -> Vec<u8> {
    let fill_hash = binary_rc4_fill_hash(pwd_bytes, salt);
    let mut int_buf = vec![0u8; 336];
    for i in (0..int_buf.len()).step_by(fill_hash.len()) {
        int_buf[i..i + fill_hash.len()].copy_from_slice(&fill_hash);
    }
    fix_to_length(&HashAlgorithm::Md5.hash(&int_buf), 5)
}

/// OC 2.3.6.2 (part 2): `MD5(baseHash || block)`.
pub fn binary_rc4_derive_key(base_hash: &[u8], block_bytes: &[u8]) -> Vec<u8> {
    fix_to_length(
        &HashAlgorithm::Md5.hash_parts(&[base_hash, block_bytes]),
        bits_to_bytes(128),
    )
}

/// Port of `RC4CryptoAPIProvider`.
pub struct Rc4CryptoApiProvider {
    header: EncryptionHeader,
    verifier: EncryptionVerifier,
    base_hash: Vec<u8>,
    enc_key_byte_size: usize,
}

impl Rc4CryptoApiProvider {
    /// `new RC4CryptoAPIProvider(channel, encodingKey, encProvBuf, pwdBytes)`.
    pub fn new(enc_prov_buf: &mut ByteBuf, pwd_bytes: &[u8]) -> Result<Rc4CryptoApiProvider> {
        let header = EncryptionHeader::read(
            enc_prov_buf,
            &[CryptoAlgorithm::Rc4],
            &[HeaderHashAlgorithm::Sha1],
        )?;
        let verifier = EncryptionVerifier::read(enc_prov_buf, header.crypto_algorithm)?;

        // OC 2.3.5.2 (part 1)
        let base_hash = ecma_standard_base_hash(pwd_bytes, &verifier.salt);
        let enc_key_byte_size = bits_to_bytes(header.key_size as usize);

        Ok(Rc4CryptoApiProvider {
            header,
            verifier,
            base_hash,
            enc_key_byte_size,
        })
    }

    /// `RC4CryptoAPIProvider.verifyPassword`.
    pub fn verify_password(&self) -> Result<bool> {
        let mut cipher = Rc4Stream::new(&self.compute_encryption_key(&[0u8; 4]))?;

        let mut verifier = self.verifier.encrypted_verifier.clone();
        cipher.process(&mut verifier);
        let mut verifier_hash = self.verifier.encrypted_verifier_hash.clone();
        cipher.process(&mut verifier_hash);

        let verifier_hash =
            fix_to_length(&verifier_hash, self.verifier.verifier_hash_size as usize);
        let test_hash = fix_to_length(
            &HashAlgorithm::Sha1.hash(&verifier),
            self.verifier.verifier_hash_size as usize,
        );
        Ok(verifier_hash == test_hash)
    }

    /// `computeEncryptionKey(blockBytes)`.
    fn compute_encryption_key(&self, block_bytes: &[u8]) -> Vec<u8> {
        // OC 2.3.5.2 (part 2)
        rc4_cryptoapi_derive_key(
            &self.base_hash,
            block_bytes,
            self.enc_key_byte_size,
            self.header.key_size,
        )
    }

    /// Decrypts one whole page in place.
    pub fn decrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let block_bytes = apply_page_number(encoding_key, 0, page_number);
        let mut cipher = Rc4Stream::new(&self.compute_encryption_key(&block_bytes))?;
        cipher.process(page);
        Ok(())
    }

    /// Encrypts one whole page in place.
    pub fn encrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let block_bytes = apply_page_number(encoding_key, 0, page_number);
        let mut cipher = Rc4Stream::new(&self.compute_encryption_key(&block_bytes))?;
        cipher.process(page);
        Ok(())
    }

    pub fn header(&self) -> &EncryptionHeader {
        &self.header
    }
}

/// Port of `OfficeBinaryDocRC4Provider`.
pub struct BinaryRc4Provider {
    enc_verifier: Vec<u8>,
    enc_verifier_hash: Vec<u8>,
    base_hash: Vec<u8>,
}

impl BinaryRc4Provider {
    /// `new OfficeBinaryDocRC4Provider(channel, encodingKey, encProvBuf,
    /// pwdBytes)`.
    pub fn new(enc_prov_buf: &mut ByteBuf, pwd_bytes: &[u8]) -> Result<BinaryRc4Provider> {
        // OC 2.3.6.1
        let salt = enc_prov_buf.get_bytes(16)?;
        let enc_verifier = enc_prov_buf.get_bytes(16)?;
        let enc_verifier_hash = enc_prov_buf.get_bytes(16)?;

        // OC 2.3.6.2 (Part 1)
        let base_hash = binary_rc4_base_hash(pwd_bytes, &salt);

        Ok(BinaryRc4Provider {
            enc_verifier,
            enc_verifier_hash,
            base_hash,
        })
    }

    /// `OfficeBinaryDocRC4Provider.verifyPassword`.
    pub fn verify_password(&self) -> Result<bool> {
        let mut cipher = Rc4Stream::new(&self.compute_encryption_key(&[0u8; 4]))?;

        let mut verifier = self.enc_verifier.clone();
        cipher.process(&mut verifier);
        let mut verifier_hash = self.enc_verifier_hash.clone();
        cipher.process(&mut verifier_hash);

        let test_hash = HashAlgorithm::Md5.hash(&verifier);
        Ok(verifier_hash == test_hash)
    }

    /// `computeEncryptionKey(blockBytes)`.
    fn compute_encryption_key(&self, block_bytes: &[u8]) -> Vec<u8> {
        // OC 2.3.6.2 (Part 2)
        binary_rc4_derive_key(&self.base_hash, block_bytes)
    }

    /// Decrypts one whole page in place.
    pub fn decrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let block_bytes = apply_page_number(encoding_key, 0, page_number);
        let mut cipher = Rc4Stream::new(&self.compute_encryption_key(&block_bytes))?;
        cipher.process(page);
        Ok(())
    }

    /// Encrypts one whole page in place.
    pub fn encrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let block_bytes = apply_page_number(encoding_key, 0, page_number);
        let mut cipher = Rc4Stream::new(&self.compute_encryption_key(&block_bytes))?;
        cipher.process(page);
        Ok(())
    }
}

/// Returns the error unchanged when the second attempt failed as well
/// (`OfficeCryptCodecHandler.create` rethrows the original RC4 error).
pub fn prefer_first_error(first: Error, _second: Error) -> Error {
    first
}
