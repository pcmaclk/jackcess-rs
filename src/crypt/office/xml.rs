//! Port of `com.healthmarketscience.jackcess.crypt.impl.office
//! .XmlEncryptionDescriptor` / `XmlEncryptionParser` and the descriptor model
//! (`CTEncryption`, `CTKeyData`, `CTKeyEncryptor(s)`, `CTDataIntegrity`,
//! `CTPasswordKeyEncryptor`).
//!
//! The agile `EncryptionInfo` is an XML document with three elements: the
//! key data (the algorithms and the content key salt), an optional
//! `dataIntegrity` element (which no `.accdb` ms access wrote carries), and
//! the key encryptors (exactly one password encryptor in every file ms access
//! writes).  The document is parsed with a namespace aware parser, like the
//! Java `DocumentBuilderFactory` the Java code uses.

use crate::error::{Error, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;

/// `XmlEncryptionParser.ENC_NS`.
pub const ENC_NS: &str = "http://schemas.microsoft.com/office/2006/encryption";
/// `XmlEncryptionParser.PWD_NS`.
pub const PWD_NS: &str = "http://schemas.microsoft.com/office/2006/keyEncryptor/password";
/// `XmlEncryptionParser.CERT_NS`.
pub const CERT_NS: &str = "http://schemas.microsoft.com/office/2006/keyEncryptor/certificate";

/// `<keyData>`.
#[derive(Clone, Debug)]
pub struct KeyData {
    pub salt_size: u64,
    pub block_size: u64,
    pub key_bits: u64,
    pub hash_size: u64,
    pub cipher_algorithm: String,
    pub cipher_chaining: String,
    pub hash_algorithm: String,
    pub salt_value: Vec<u8>,
}

/// `<p:encryptedKey>`.
#[derive(Clone, Debug)]
pub struct PasswordKeyEncryptor {
    pub salt_size: u64,
    pub block_size: u64,
    pub key_bits: u64,
    pub hash_size: u64,
    pub cipher_algorithm: String,
    pub cipher_chaining: String,
    pub hash_algorithm: String,
    pub salt_value: Vec<u8>,
    pub spin_count: u64,
    pub encrypted_verifier_hash_input: Vec<u8>,
    pub encrypted_verifier_hash_value: Vec<u8>,
    pub encrypted_key_value: Vec<u8>,
}

/// `<keyEncryptor>`: the uri plus the one encryptor it carries.
#[derive(Clone, Debug)]
pub struct KeyEncryptor {
    pub uri: String,
    pub password: Option<PasswordKeyEncryptor>,
    pub certificate: bool,
}

/// `<dataIntegrity>`.
#[derive(Clone, Debug)]
pub struct DataIntegrity {
    pub encrypted_hmac_key: Vec<u8>,
    pub encrypted_hmac_value: Vec<u8>,
}

/// `<encryption>`.
#[derive(Clone, Debug)]
pub struct EncryptionDescriptor {
    pub key_data: KeyData,
    pub data_integrity: Option<DataIntegrity>,
    pub key_encryptors: Vec<KeyEncryptor>,
}

impl EncryptionDescriptor {
    /// The single password key encryptor, which is what
    /// `AgileEncryptionProvider` requires.
    pub fn password_key_encryptor(&self) -> Option<&PasswordKeyEncryptor> {
        if self.key_encryptors.len() != 1 {
            return None;
        }
        self.key_encryptors[0].password.as_ref()
    }
}

/// `XmlEncryptionDescriptor.parseEncryptionDescriptor`.
pub fn parse_encryption_descriptor(xml_bytes: &[u8]) -> Result<EncryptionDescriptor> {
    let text = std::str::from_utf8(xml_bytes)
        .map_err(|e| Error::Unsupported(format!("Failed parsing encryption descriptor: {}", e)))?;
    let doc = roxmltree::Document::parse(text)
        .map_err(|e| Error::Unsupported(format!("Failed parsing encryption descriptor: {}", e)))?;

    let root = doc.root_element();
    if root.tag_name().name() != "encryption" || root.tag_name().namespace() != Some(ENC_NS) {
        return Err(Error::Unsupported(format!(
            "Unexpected xml config {}",
            root.tag_name().name()
        )));
    }

    let key_data_el = find_element(root, ENC_NS, "keyData", true)?.expect("required");
    let data_integrity_el = find_element(root, ENC_NS, "dataIntegrity", false)?;
    let key_encryptors_el = find_element(root, ENC_NS, "keyEncryptors", true)?.expect("required");

    let mut key_encryptors = Vec::new();
    for el in find_elements(key_encryptors_el, ENC_NS, "keyEncryptor") {
        key_encryptors.push(parse_key_encryptor(el)?);
    }

    Ok(EncryptionDescriptor {
        key_data: parse_key_data(key_data_el)?,
        data_integrity: match data_integrity_el {
            Some(el) => Some(parse_data_integrity(el)?),
            None => None,
        },
        key_encryptors,
    })
}

fn parse_key_data(el: roxmltree::Node) -> Result<KeyData> {
    Ok(KeyData {
        salt_size: get_long_attribute(el, "saltSize")? as u64,
        block_size: get_long_attribute(el, "blockSize")? as u64,
        key_bits: get_long_attribute(el, "keyBits")? as u64,
        hash_size: get_long_attribute(el, "hashSize")? as u64,
        cipher_algorithm: get_string_attribute(el, "cipherAlgorithm")?,
        cipher_chaining: get_string_attribute(el, "cipherChaining")?,
        hash_algorithm: get_string_attribute(el, "hashAlgorithm")?,
        salt_value: get_base64_attribute(el, "saltValue")?,
    })
}

fn parse_data_integrity(el: roxmltree::Node) -> Result<DataIntegrity> {
    Ok(DataIntegrity {
        encrypted_hmac_key: get_base64_attribute(el, "encryptedHmacKey")?,
        encrypted_hmac_value: get_base64_attribute(el, "encryptedHmacValue")?,
    })
}

fn parse_key_encryptor(el: roxmltree::Node) -> Result<KeyEncryptor> {
    let uri = get_string_attribute(el, "uri")?;
    if uri == PWD_NS {
        Ok(KeyEncryptor {
            uri,
            password: Some(parse_password_key_encryptor(el)?),
            certificate: false,
        })
    } else if uri == CERT_NS {
        // the certificate encryptor is parsed by Java but cannot be used
        // without the certificate, so the values are not held here
        find_element(el, CERT_NS, "encryptedKey", true)?;
        Ok(KeyEncryptor {
            uri,
            password: None,
            certificate: true,
        })
    } else {
        Err(Error::Unsupported(format!(
            "Unexpected xml config {} under {}",
            uri,
            el.tag_name().name()
        )))
    }
}

fn parse_password_key_encryptor(el: roxmltree::Node) -> Result<PasswordKeyEncryptor> {
    let pwd_el = find_element(el, PWD_NS, "encryptedKey", true)?.expect("required");
    Ok(PasswordKeyEncryptor {
        salt_size: get_long_attribute(pwd_el, "saltSize")? as u64,
        block_size: get_long_attribute(pwd_el, "blockSize")? as u64,
        key_bits: get_long_attribute(pwd_el, "keyBits")? as u64,
        hash_size: get_long_attribute(pwd_el, "hashSize")? as u64,
        cipher_algorithm: get_string_attribute(pwd_el, "cipherAlgorithm")?,
        cipher_chaining: get_string_attribute(pwd_el, "cipherChaining")?,
        hash_algorithm: get_string_attribute(pwd_el, "hashAlgorithm")?,
        salt_value: get_base64_attribute(pwd_el, "saltValue")?,
        spin_count: get_long_attribute(pwd_el, "spinCount")? as u64,
        encrypted_verifier_hash_input: get_base64_attribute(pwd_el, "encryptedVerifierHashInput")?,
        encrypted_verifier_hash_value: get_base64_attribute(pwd_el, "encryptedVerifierHashValue")?,
        encrypted_key_value: get_base64_attribute(pwd_el, "encryptedKeyValue")?,
    })
}

/// `XmlEncryptionParser.getElement`: the first descendant of the given parent
/// with the given namespace and local name.
fn find_element<'a>(
    parent: roxmltree::Node<'a, 'a>,
    ns: &str,
    local_name: &str,
    required: bool,
) -> Result<Option<roxmltree::Node<'a, 'a>>> {
    let found = parent.descendants().find(|n| {
        n.is_element() && n.tag_name().name() == local_name && n.tag_name().namespace() == Some(ns)
    });
    if found.is_none() && required {
        return Err(Error::Unsupported(format!(
            "Could not find xml config {} under {}",
            local_name,
            parent.tag_name().name()
        )));
    }
    Ok(found)
}

/// `XmlEncryptionParser.getElements`.
fn find_elements<'a>(
    parent: roxmltree::Node<'a, 'a>,
    ns: &str,
    local_name: &str,
) -> Vec<roxmltree::Node<'a, 'a>> {
    parent
        .descendants()
        .filter(|n| {
            n.is_element()
                && n.tag_name().name() == local_name
                && n.tag_name().namespace() == Some(ns)
        })
        .collect()
}

/// `XmlEncryptionParser.getLongAttribute`.
fn get_long_attribute(el: roxmltree::Node, local_name: &str) -> Result<i64> {
    let value = get_string_attribute(el, local_name)?;
    value.parse::<i64>().map_err(|_| {
        Error::Unsupported(format!(
            "Could not parse {} \"{}\" under {}",
            local_name,
            value,
            el.tag_name().name()
        ))
    })
}

/// `XmlEncryptionParser.getStringAttribute`, including the trim to null.
fn get_string_attribute(el: roxmltree::Node, local_name: &str) -> Result<String> {
    let value = el.attribute(local_name).map(str::trim).unwrap_or("");
    if value.is_empty() {
        return Err(Error::Unsupported(format!(
            "Could not find xml config {} under {}",
            local_name,
            el.tag_name().name()
        )));
    }
    Ok(value.to_string())
}

/// `XmlEncryptionParser.getBase64Attribute`.
fn get_base64_attribute(el: roxmltree::Node, local_name: &str) -> Result<Vec<u8>> {
    let value = get_string_attribute(el, local_name)?;
    B64.decode(value.as_bytes()).map_err(|e| {
        Error::Unsupported(format!(
            "Could not parse {} under {}: {}",
            local_name,
            el.tag_name().name(),
            e
        ))
    })
}

/// `Base64.getEncoder().encodeToString`.
pub fn encode_base64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}
