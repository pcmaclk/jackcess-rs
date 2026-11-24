//! Port of `com.healthmarketscience.jackcess.crypt.AgileEncryptionConfig`,
//! `impl.office.AgileEncryptionProvider` and
//! `impl.office.AgileEncryptionWriter`.
//!
//! Agile encryption carries its algorithms in the `EncryptionInfo` XML
//! descriptor, so a reader honours whatever it finds; a writer uses one of the
//! two combinations ms access writes (`OFFICE_2010` and `OFFICE_2013`).
//!
//! The three wrapped values (verifier hash input, verifier hash value and the
//! content key) are encrypted with AES-CBC under keys derived from the
//! password and the password salt, and every page is encrypted with the
//! content key under an IV derived from the key data salt and the encoding
//! key xored with the page number.

use crate::bytebuf::ByteBuf;
use crate::crypt::codec::apply_page_number;
use crate::crypt::office::crypto::{
    bits_to_bytes, fix_to_length, fix_to_length_with, iterate_hash, random_bytes, BlockCipherState,
    CipherAlgorithm, CipherChaining, HashAlgorithm,
};
use crate::crypt::office::xml::{
    encode_base64, parse_encryption_descriptor, KeyData, PasswordKeyEncryptor, ENC_NS, PWD_NS,
};
use crate::error::{Error, Result};

/// `AgileEncryptionProvider.RESERVED_VAL`.
pub const RESERVED_VAL: i32 = 0x40;

/// `AgileEncryptionProvider.VERIFIER_INPUT_BLOCK`.
pub const VERIFIER_INPUT_BLOCK: [u8; 8] = [0xfe, 0xa7, 0xd2, 0x76, 0x3b, 0x4b, 0x9e, 0x79];
/// `AgileEncryptionProvider.VERIFIER_VALUE_BLOCK`.
pub const VERIFIER_VALUE_BLOCK: [u8; 8] = [0xd7, 0xaa, 0x0f, 0x6d, 0x30, 0x61, 0x34, 0x4e];
/// `AgileEncryptionProvider.KEY_VALUE_BLOCK`.
pub const KEY_VALUE_BLOCK: [u8; 8] = [0x14, 0x6e, 0x0b, 0xe7, 0xab, 0xac, 0xd0, 0xd6];

/// `AgileEncryptionProvider.PAD_BYTE`.
const PAD_BYTE: u8 = 0x36;

/// `AgileEncryptionProvider.deriveKey`: builds the key for one block constant
/// out of the password and the salt.  OC 2.3.4.11.
pub fn derive_key(
    digest: HashAlgorithm,
    pwd_bytes: &[u8],
    salt: &[u8],
    spin_count: u32,
    block_bytes: &[u8],
    key_byte_len: usize,
) -> Vec<u8> {
    let base_hash = digest.hash_parts(&[salt, pwd_bytes]);
    let iter_hash = iterate_hash(digest, &base_hash, spin_count);
    let final_hash = digest.hash_parts(&[&iter_hash, block_bytes]);
    fix_to_length_with(&final_hash, key_byte_len, PAD_BYTE)
}

/// `AgileEncryptionProvider.deriveIV`: builds the IV for one block constant,
/// or returns the salt itself where there is no block constant.  OC 2.3.4.12.
pub fn derive_iv(
    digest: HashAlgorithm,
    salt: &[u8],
    block_bytes: Option<&[u8]>,
    block_size: usize,
) -> Vec<u8> {
    let iv_bytes = match block_bytes {
        Some(block) => digest.hash_parts(&[salt, block]),
        None => salt.to_vec(),
    };
    fix_to_length_with(&iv_bytes, block_size, PAD_BYTE)
}

/// `AgileEncryptionProvider.padToBlockSize`: rounds the given hash up to a
/// whole number of cipher blocks, zero padded (the Java helper's
/// `fixToLength` default).
pub fn pad_to_block_size(bytes: &[u8], block_size: usize) -> Vec<u8> {
    if block_size == 0 || bytes.len().is_multiple_of(block_size) {
        return bytes.to_vec();
    }
    let len = bytes.len().div_ceil(block_size) * block_size;
    fix_to_length(bytes, len)
}

/// Port of `AgileEncryptionConfig`: the algorithms agile encryption is written
/// with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgileEncryptionConfig {
    cipher: CipherAlgorithm,
    chaining: CipherChaining,
    key_bits: u32,
    hash: HashAlgorithm,
    spin_count: u32,
    salt_size: usize,
}

impl AgileEncryptionConfig {
    /// The spin count ms access writes in both of its profiles.
    pub const DEFAULT_SPIN_COUNT: u32 = 100_000;
    /// The salt size ms access writes in both of its profiles.
    pub const DEFAULT_SALT_SIZE: usize = 16;
    /// The key sizes which can be written.  192 bits is missing because the
    /// key value is encrypted whole and unpadded, and 24 bytes is not a whole
    /// number of 16 byte blocks.
    const KEY_BITS: [u32; 2] = [128, 256];

    /// `AgileEncryptionConfig.OFFICE_2010`: what ms access writes by default,
    /// and still writes today.
    pub fn office_2010() -> AgileEncryptionConfig {
        AgileEncryptionConfig {
            cipher: CipherAlgorithm::Aes,
            chaining: CipherChaining::Cbc,
            key_bits: 128,
            hash: HashAlgorithm::Sha1,
            spin_count: Self::DEFAULT_SPIN_COUNT,
            salt_size: Self::DEFAULT_SALT_SIZE,
        }
    }

    /// `AgileEncryptionConfig.OFFICE_2013`: the stronger profile ms access
    /// 2013 writes.
    pub fn office_2013() -> AgileEncryptionConfig {
        AgileEncryptionConfig {
            cipher: CipherAlgorithm::Aes,
            chaining: CipherChaining::Cbc,
            key_bits: 256,
            hash: HashAlgorithm::Sha512,
            spin_count: Self::DEFAULT_SPIN_COUNT,
            salt_size: Self::DEFAULT_SALT_SIZE,
        }
    }

    /// `new AgileEncryptionConfig(cipher, chaining, keyBits, hash,
    /// spinCount, saltSize)`, with the Java validation rules.
    pub fn new(
        cipher: CipherAlgorithm,
        chaining: CipherChaining,
        key_bits: u32,
        hash: HashAlgorithm,
        spin_count: u32,
        salt_size: usize,
    ) -> Result<AgileEncryptionConfig> {
        if chaining != CipherChaining::Cbc {
            return Err(Error::IllegalArgument(format!(
                "Unsupported cipher chaining {}, only ChainingModeCBC can be written",
                chaining.xml_name()
            )));
        }
        if !matches!(
            hash,
            HashAlgorithm::Sha1
                | HashAlgorithm::Sha256
                | HashAlgorithm::Sha384
                | HashAlgorithm::Sha512
        ) {
            return Err(Error::IllegalArgument(format!(
                "Unsupported hash algorithm {}, expected one of SHA1, SHA256, SHA384, SHA512",
                hash
            )));
        }
        if !Self::KEY_BITS.contains(&key_bits) {
            return Err(Error::IllegalArgument(format!(
                "Unsupported key size {}, expected one of {:?}",
                key_bits,
                Self::KEY_BITS
            )));
        }
        // the password salt is the IV the wrapped values are encrypted under,
        // and the read path hands it to the cipher as it stands, which accepts
        // only an IV of exactly the block size
        if salt_size != cipher.block_size() {
            return Err(Error::IllegalArgument(format!(
                "Salt size must be the {} byte block size of {}, got {}",
                cipher.block_size(),
                cipher,
                salt_size
            )));
        }
        // the key is the first bytes of a hash, and a hash shorter than the key
        // is padded out with a fixed byte
        if hash.size() < bits_to_bytes(key_bits as usize) {
            return Err(Error::IllegalArgument(format!(
                "A {} bit key needs a hash longer than {}, which gives {} bytes",
                key_bits,
                hash,
                hash.size()
            )));
        }
        Ok(AgileEncryptionConfig {
            cipher,
            chaining,
            key_bits,
            hash,
            spin_count,
            salt_size,
        })
    }

    pub fn cipher(&self) -> CipherAlgorithm {
        self.cipher
    }

    pub fn chaining(&self) -> CipherChaining {
        self.chaining
    }

    pub fn key_bits(&self) -> u32 {
        self.key_bits
    }

    pub fn hash(&self) -> HashAlgorithm {
        self.hash
    }

    pub fn spin_count(&self) -> u32 {
        self.spin_count
    }

    pub fn salt_size(&self) -> usize {
        self.salt_size
    }

    /// The cipher block size, which follows from the cipher.
    pub fn block_size(&self) -> usize {
        self.cipher.block_size()
    }

    /// The hash length, which follows from the hash.
    pub fn hash_size(&self) -> usize {
        self.hash.size()
    }

    pub fn key_byte_len(&self) -> usize {
        bits_to_bytes(self.key_bits as usize)
    }
}

impl std::fmt::Display for AgileEncryptionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // matches `AgileEncryptionConfig.toString`, which prints the enum
        // constant names ("CBC", not the XML "ChainingModeCBC")
        write!(
            f,
            "AgileEncryptionConfig[{} CBC {} bits, {}, spinCount {}, saltSize {}]",
            self.cipher, self.key_bits, self.hash, self.spin_count, self.salt_size
        )
    }
}

/// The key material an agile writer works with.  Java generates it from
/// `SecureRandom`; keeping it in a value lets a caller pin it, which is the
/// only way the (otherwise random) encrypted output can be compared.
#[derive(Clone, Debug)]
pub struct AgileKeyMaterial {
    pub key_data_salt: Vec<u8>,
    pub pwd_salt: Vec<u8>,
    pub key_value: Vec<u8>,
    pub verifier_input: Vec<u8>,
}

impl AgileKeyMaterial {
    /// Generates fresh key material for the given configuration.
    pub fn random(config: &AgileEncryptionConfig) -> Result<AgileKeyMaterial> {
        Ok(AgileKeyMaterial {
            key_data_salt: random_bytes(config.salt_size())?,
            pwd_salt: random_bytes(config.salt_size())?,
            key_value: random_bytes(config.key_byte_len())?,
            verifier_input: random_bytes(config.salt_size())?,
        })
    }
}

/// Port of `AgileEncryptionWriter`: builds the agile key material and the
/// descriptor which carries it.
#[derive(Clone, Debug)]
pub struct AgileEncryptionWriter {
    config: AgileEncryptionConfig,
    pwd_bytes: Vec<u8>,
    material: AgileKeyMaterial,
}

impl AgileEncryptionWriter {
    /// `new AgileEncryptionWriter(config, password)`: generates fresh key
    /// material.
    pub fn new(config: AgileEncryptionConfig, password: &str) -> Result<AgileEncryptionWriter> {
        let material = AgileKeyMaterial::random(&config)?;
        AgileEncryptionWriter::with_key_material(config, password, material)
    }

    /// `new AgileEncryptionWriter(config, password, keyDataSalt, pwdSalt,
    /// keyValue, verifierInput)`: uses the given key material rather than
    /// generating it.
    pub fn with_key_material(
        config: AgileEncryptionConfig,
        password: &str,
        material: AgileKeyMaterial,
    ) -> Result<AgileEncryptionWriter> {
        if material.key_data_salt.len() != config.salt_size()
            || material.pwd_salt.len() != config.salt_size()
        {
            return Err(Error::IllegalArgument(format!(
                "key material salts must be {} bytes, got {} and {}",
                config.salt_size(),
                material.key_data_salt.len(),
                material.pwd_salt.len()
            )));
        }
        if material.key_value.len() != config.key_byte_len() {
            return Err(Error::IllegalArgument(format!(
                "key material key must be {} bytes, got {}",
                config.key_byte_len(),
                material.key_value.len()
            )));
        }
        Ok(AgileEncryptionWriter {
            pwd_bytes: super::get_password_bytes(Some(password)),
            config,
            material,
        })
    }

    /// Returns the key the pages are encrypted with.
    pub fn key_value(&self) -> &[u8] {
        &self.material.key_value
    }

    /// Returns the salt which also serves as the page cipher IV base.
    pub fn key_data_salt(&self) -> &[u8] {
        &self.material.key_data_salt
    }

    pub fn verifier_input(&self) -> &[u8] {
        &self.material.verifier_input
    }

    pub fn config(&self) -> &AgileEncryptionConfig {
        &self.config
    }

    /// `AgileEncryptionWriter.encryptVerifierHashInput`.
    pub fn encrypt_verifier_hash_input(&self) -> Result<Vec<u8>> {
        self.wrap(&VERIFIER_INPUT_BLOCK, &self.material.verifier_input)
    }

    /// `AgileEncryptionWriter.encryptVerifierHashValue`.
    pub fn encrypt_verifier_hash_value(&self) -> Result<Vec<u8>> {
        let hashed = pad_to_block_size(
            &self.config.hash().hash(&self.material.verifier_input),
            self.config.block_size(),
        );
        self.wrap(&VERIFIER_VALUE_BLOCK, &hashed)
    }

    /// `AgileEncryptionWriter.encryptKeyValue`.
    pub fn encrypt_key_value(&self) -> Result<Vec<u8>> {
        self.wrap(&KEY_VALUE_BLOCK, &self.material.key_value)
    }

    /// `AgileEncryptionWriter.unwrapKeyValue`.
    pub fn unwrap_key_value(&self, wrapped: &[u8]) -> Result<Vec<u8>> {
        self.crypt(&KEY_VALUE_BLOCK, wrapped, false)
    }

    /// `AgileEncryptionWriter.unwrapVerifierHashInput`.
    pub fn unwrap_verifier_hash_input(&self, wrapped: &[u8]) -> Result<Vec<u8>> {
        self.crypt(&VERIFIER_INPUT_BLOCK, wrapped, false)
    }

    /// `AgileEncryptionWriter.createDescriptor`: the descriptor xml which page
    /// 0 carries after the version and the reserved value.
    pub fn create_descriptor(&self) -> Result<Vec<u8>> {
        let verifier_hash_input = self.encrypt_verifier_hash_input()?;
        let verifier_hash_value = self.encrypt_verifier_hash_value()?;
        let key_value = self.encrypt_key_value()?;

        let mut xml = String::new();
        xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n");
        xml.push_str(&format!(
            "<encryption xmlns=\"{}\" xmlns:p=\"{}\">",
            ENC_NS, PWD_NS
        ));
        xml.push_str(&format!(
            "<keyData saltSize=\"{}\" blockSize=\"{}\" keyBits=\"{}\" hashSize=\"{}\" \
             cipherAlgorithm=\"{}\" cipherChaining=\"{}\" hashAlgorithm=\"{}\" saltValue=\"{}\"/>",
            self.config.salt_size(),
            self.config.block_size(),
            self.config.key_bits(),
            self.config.hash_size(),
            self.config.cipher().xml_name(),
            self.config.chaining().xml_name(),
            self.config.hash().xml_name(),
            encode_base64(&self.material.key_data_salt)
        ));
        xml.push_str("<keyEncryptors>");
        xml.push_str(&format!("<keyEncryptor uri=\"{}\">", PWD_NS));
        xml.push_str(&format!(
            "<p:encryptedKey spinCount=\"{}\" saltSize=\"{}\" blockSize=\"{}\" keyBits=\"{}\" \
             hashSize=\"{}\" cipherAlgorithm=\"{}\" cipherChaining=\"{}\" hashAlgorithm=\"{}\" \
             saltValue=\"{}\" encryptedVerifierHashInput=\"{}\" \
             encryptedVerifierHashValue=\"{}\" encryptedKeyValue=\"{}\"/>",
            self.config.spin_count(),
            self.config.salt_size(),
            self.config.block_size(),
            self.config.key_bits(),
            self.config.hash_size(),
            self.config.cipher().xml_name(),
            self.config.chaining().xml_name(),
            self.config.hash().xml_name(),
            encode_base64(&self.material.pwd_salt),
            encode_base64(&verifier_hash_input),
            encode_base64(&verifier_hash_value),
            encode_base64(&key_value)
        ));
        xml.push_str("</keyEncryptor>");
        xml.push_str("</keyEncryptors>");
        xml.push_str("</encryption>");
        Ok(xml.into_bytes())
    }

    /// `AgileEncryptionWriter.encryptPage`: encrypts one page under the
    /// content key.  The IV comes from the key data salt and the block bytes,
    /// which are the encoding key xored with the page number.
    pub fn encrypt_page(
        config: &AgileEncryptionConfig,
        key_value: &[u8],
        salt: &[u8],
        block_bytes: &[u8],
        page_bytes: &mut [u8],
    ) -> Result<()> {
        let iv = derive_iv(config.hash(), salt, Some(block_bytes), config.block_size());
        let mut cipher =
            BlockCipherState::new(config.cipher(), config.chaining(), key_value, Some(&iv))?;
        cipher.encrypt(page_bytes)
    }

    /// Encrypts one of the three values under a key derived for its block
    /// constant, with the password salt as the IV.
    fn wrap(&self, block_bytes: &[u8], value: &[u8]) -> Result<Vec<u8>> {
        self.crypt(block_bytes, value, true)
    }

    fn crypt(&self, block_bytes: &[u8], value: &[u8], encrypt: bool) -> Result<Vec<u8>> {
        let key = derive_key(
            self.config.hash(),
            &self.pwd_bytes,
            &self.material.pwd_salt,
            self.config.spin_count(),
            block_bytes,
            self.config.key_byte_len(),
        );
        let iv = derive_iv(
            self.config.hash(),
            &self.material.pwd_salt,
            None,
            self.config.block_size(),
        );
        let mut cipher = BlockCipherState::new(
            self.config.cipher(),
            self.config.chaining(),
            &key,
            Some(&iv),
        )?;
        let mut out = value.to_vec();
        if encrypt {
            cipher.encrypt(&mut out)?;
        } else {
            cipher.decrypt(&mut out)?;
        }
        Ok(out)
    }
}

/// Port of `AgileEncryptionProvider`: the read path.
pub struct AgileEncryptionProvider {
    key_data: KeyData,
    pwd_key_enc: PasswordKeyEncryptor,
    key_value: Vec<u8>,
    pwd_hash: HashAlgorithm,
    pwd_cipher: CipherAlgorithm,
    pwd_chaining: CipherChaining,
    crypt_hash: HashAlgorithm,
    crypt_cipher: CipherAlgorithm,
    crypt_chaining: CipherChaining,
}

impl AgileEncryptionProvider {
    /// `new AgileEncryptionProvider(channel, encodingKey, encProvBuf,
    /// pwdBytes)`: the reserved value and the XML descriptor are read from the
    /// provider buffer.
    pub fn new(enc_prov_buf: &mut ByteBuf, pwd_bytes: &[u8]) -> Result<AgileEncryptionProvider> {
        let reserved_val = enc_prov_buf.get_i32()?;
        if reserved_val != RESERVED_VAL {
            return Err(Error::Unsupported(format!(
                "Unexpected reserved value {}",
                reserved_val
            )));
        }

        let xml_bytes = enc_prov_buf.get_bytes(enc_prov_buf.remaining())?;
        let encrypt_desc = parse_encryption_descriptor(&xml_bytes)?;

        let pwd_key_enc = encrypt_desc
            .password_key_encryptor()
            .ok_or_else(|| Error::Unsupported("Missing or unexpected key encryptor".into()))?;
        let pwd_key_enc = pwd_key_enc.clone();

        let pwd_hash = HashAlgorithm::from_xml_name(&pwd_key_enc.hash_algorithm)?;
        let pwd_cipher = CipherAlgorithm::from_xml_name(&pwd_key_enc.cipher_algorithm)?;
        let pwd_chaining = CipherChaining::from_xml_name(&pwd_key_enc.cipher_chaining)?;
        let crypt_hash = HashAlgorithm::from_xml_name(&encrypt_desc.key_data.hash_algorithm)?;
        let crypt_cipher = CipherAlgorithm::from_xml_name(&encrypt_desc.key_data.cipher_algorithm)?;
        let crypt_chaining = CipherChaining::from_xml_name(&encrypt_desc.key_data.cipher_chaining)?;

        let key_value =
            decrypt_key_value(pwd_hash, pwd_cipher, pwd_chaining, pwd_bytes, &pwd_key_enc)?;

        Ok(AgileEncryptionProvider {
            key_data: encrypt_desc.key_data,
            pwd_key_enc,
            key_value,
            pwd_hash,
            pwd_cipher,
            pwd_chaining,
            crypt_hash,
            crypt_cipher,
            crypt_chaining,
        })
    }

    /// `AgileEncryptionProvider.verifyPassword`.
    pub fn verify_password(&self, pwd_bytes: &[u8]) -> Result<bool> {
        let verifier = self.decrypt_verifier_hash_input(pwd_bytes)?;
        let verifier_hash = self.decrypt_verifier_hash_value(pwd_bytes)?;

        let test_hash = pad_to_block_size(
            &self.pwd_hash.hash(&verifier),
            self.pwd_key_enc.block_size as usize,
        );

        Ok(verifier_hash == test_hash)
    }

    fn decrypt_verifier_hash_input(&self, pwd_bytes: &[u8]) -> Result<Vec<u8>> {
        let key = self.crypt_derive_key(pwd_bytes, &VERIFIER_INPUT_BLOCK);
        self.block_decrypt_bytes(&key, &self.pwd_key_enc.encrypted_verifier_hash_input)
    }

    fn decrypt_verifier_hash_value(&self, pwd_bytes: &[u8]) -> Result<Vec<u8>> {
        let key = self.crypt_derive_key(pwd_bytes, &VERIFIER_VALUE_BLOCK);
        self.block_decrypt_bytes(&key, &self.pwd_key_enc.encrypted_verifier_hash_value)
    }

    fn crypt_derive_key(&self, pwd_bytes: &[u8], block_bytes: &[u8]) -> Vec<u8> {
        derive_key(
            self.pwd_hash,
            pwd_bytes,
            &self.pwd_key_enc.salt_value,
            self.pwd_key_enc.spin_count as u32,
            block_bytes,
            bits_to_bytes(self.pwd_key_enc.key_bits as usize),
        )
    }

    /// `BaseCryptCodecHandler.blockDecryptBytes(key, iv, encBytes)`: the
    /// password salt is the IV.
    fn block_decrypt_bytes(&self, key: &[u8], enc_bytes: &[u8]) -> Result<Vec<u8>> {
        let mut cipher = BlockCipherState::new(
            self.pwd_cipher,
            self.pwd_chaining,
            key,
            Some(&self.pwd_key_enc.salt_value),
        )?;
        let mut out = enc_bytes.to_vec();
        cipher.decrypt(&mut out)?;
        Ok(out)
    }

    /// `AgileEncryptionProvider.computeCipherParams`: the content key and the
    /// IV for the given page.
    pub fn cipher_params(
        &self,
        encoding_key: &[u8],
        page_number: i32,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let block_bytes = apply_page_number(encoding_key, 0, page_number);
        let iv = derive_iv(
            self.crypt_hash,
            &self.key_data.salt_value,
            Some(&block_bytes),
            self.key_data.block_size as usize,
        );
        Ok((self.key_value.clone(), iv))
    }

    /// Decrypts one whole page in place.
    pub fn decrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let (key, iv) = self.cipher_params(encoding_key, page_number)?;
        let mut cipher =
            BlockCipherState::new(self.crypt_cipher, self.crypt_chaining, &key, Some(&iv))?;
        cipher.decrypt(page)
    }

    /// Encrypts one whole page in place.
    pub fn encrypt_page(
        &self,
        encoding_key: &[u8],
        page_number: i32,
        page: &mut [u8],
    ) -> Result<()> {
        let (key, iv) = self.cipher_params(encoding_key, page_number)?;
        let mut cipher =
            BlockCipherState::new(self.crypt_cipher, self.crypt_chaining, &key, Some(&iv))?;
        cipher.encrypt(page)
    }

    pub fn key_value(&self) -> &[u8] {
        &self.key_value
    }
}

/// `AgileEncryptionProvider.decryptKeyValue`.
fn decrypt_key_value(
    pwd_hash: HashAlgorithm,
    pwd_cipher: CipherAlgorithm,
    pwd_chaining: CipherChaining,
    pwd_bytes: &[u8],
    pwd_key_enc: &PasswordKeyEncryptor,
) -> Result<Vec<u8>> {
    let key = derive_key(
        pwd_hash,
        pwd_bytes,
        &pwd_key_enc.salt_value,
        pwd_key_enc.spin_count as u32,
        &KEY_VALUE_BLOCK,
        bits_to_bytes(pwd_key_enc.key_bits as usize),
    );
    let mut cipher = BlockCipherState::new(
        pwd_cipher,
        pwd_chaining,
        &key,
        Some(&pwd_key_enc.salt_value),
    )?;
    let mut out = pwd_key_enc.encrypted_key_value.clone();
    cipher.decrypt(&mut out)?;
    Ok(out)
}
