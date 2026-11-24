//! Port of the Office (`.accdb`) encryption support of jackcess-encrypt:
//! the `OfficeCryptCodecHandler` which reads an encrypted container, the agile
//! and ECMA standard providers, the older RC4 providers, and
//! `OfficeEncryptionHandler` which adds and removes the encryption.
//!
//! Reading is supported for agile (4,4), ECMA standard (3,2 / 4,2 with the AES
//! flag), the non-standard AES variant (the same, zero hash iterations), RC4
//! CryptoAPI (2,2 without the AES flag) and Office binary RC4 (1,1).  Writing
//! is supported for agile only (`OFFICE_2010` and `OFFICE_2013`), which is
//! what a current ms access writes and what jackcess-encrypt can produce.
//! Removing the encryption works for every scheme the reader supports.

pub mod agile;
pub mod crypto;
pub mod encryption_handler;
pub mod header;
pub mod provider;
pub mod standard;
pub mod xml;

pub use agile::{
    derive_iv, derive_key, pad_to_block_size, AgileEncryptionConfig, AgileEncryptionProvider,
    AgileEncryptionWriter, AgileKeyMaterial,
};
pub use encryption_handler::OfficeEncryptionHandler;
pub use header::{
    CryptoAlgorithm, EncryptionHeader, EncryptionVerifier, HeaderHashAlgorithm, FAES_FLAG,
    FCRYPTO_API_FLAG,
};
pub use provider::{
    create_handler, get_password_bytes, OfficeCryptCodecHandler, CRYPT_STRUCTURE_OFFSET,
    MAX_PASSWORD_LEN,
};
pub use standard::{
    binary_rc4_base_hash, binary_rc4_derive_key, binary_rc4_fill_hash, ecma_standard_base_hash,
    ecma_standard_derive_key, ecma_standard_iter_hash, rc4_cryptoapi_derive_key, BinaryRc4Provider,
    EcmaStandardProvider, Rc4CryptoApiProvider, HASH_ITERATIONS, NON_STANDARD_HASH_ITERATIONS,
};
#[cfg(test)]
#[cfg(all(test, feature = "fixtures"))]
mod tests;

pub use xml::{
    parse_encryption_descriptor, DataIntegrity, EncryptionDescriptor, KeyData, KeyEncryptor,
    PasswordKeyEncryptor, CERT_NS, ENC_NS, PWD_NS,
};
