//! Fixtures for the office encryption port, produced by the Java library
//! (`java-src/OfficeCryptGen.java fixtures`).  Everything here is
//! deterministic: the agile key material, the password and the page contents
//! are fixed, so the wrapped values, the XML descriptor, the page ciphertext
//! and the standard / RC4 key derivations can be compared with the values
//! jackcess itself computes.  See `tools/office_crypt_fixtures.txt`.

use super::agile::{
    AgileEncryptionConfig, AgileEncryptionProvider, AgileEncryptionWriter, AgileKeyMaterial,
    KEY_VALUE_BLOCK, VERIFIER_INPUT_BLOCK, VERIFIER_VALUE_BLOCK,
};
use super::crypto::{
    fix_to_length, BlockCipherState, CipherAlgorithm, CipherChaining, HashAlgorithm,
};
use super::provider::get_password_bytes;
use super::standard::{
    binary_rc4_base_hash, binary_rc4_derive_key, binary_rc4_fill_hash, ecma_standard_base_hash,
    ecma_standard_derive_key, ecma_standard_iter_hash, rc4_cryptoapi_derive_key,
    EcmaStandardProvider, HASH_ITERATIONS,
};
use crate::bytebuf::ByteBuf;
use crate::crypt::codec::apply_page_number;

const PASSWORD: &str = "s3cret-Pa55";

fn fixed_material(config: &AgileEncryptionConfig) -> AgileKeyMaterial {
    AgileKeyMaterial {
        key_data_salt: seq(0x00, 16),
        pwd_salt: seq(0x10, 16),
        key_value: seq(0x20, config.key_byte_len()),
        verifier_input: seq(0x30, 16),
    }
}

fn seq(start: u8, len: usize) -> Vec<u8> {
    (0..len).map(|i| start.wrapping_add(i as u8)).collect()
}

fn writer(config: AgileEncryptionConfig) -> AgileEncryptionWriter {
    AgileEncryptionWriter::with_key_material(config.clone(), PASSWORD, fixed_material(&config))
        .unwrap()
}

/// The 2010 profile's descriptor, byte for byte what `AgileEncryptionWriter
/// .createDescriptor` writes for the fixed key material above.
const DESCRIPTOR_2010: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<encryption xmlns=\"http://schemas.microsoft.com/office/2006/encryption\" xmlns:p=\"http://schemas.microsoft.com/office/2006/keyEncryptor/password\">\
<keyData saltSize=\"16\" blockSize=\"16\" keyBits=\"128\" hashSize=\"20\" cipherAlgorithm=\"AES\" cipherChaining=\"ChainingModeCBC\" hashAlgorithm=\"SHA1\" saltValue=\"AAECAwQFBgcICQoLDA0ODw==\"/>\
<keyEncryptors><keyEncryptor uri=\"http://schemas.microsoft.com/office/2006/keyEncryptor/password\">\
<p:encryptedKey spinCount=\"100000\" saltSize=\"16\" blockSize=\"16\" keyBits=\"128\" hashSize=\"20\" cipherAlgorithm=\"AES\" cipherChaining=\"ChainingModeCBC\" hashAlgorithm=\"SHA1\" saltValue=\"EBESExQVFhcYGRobHB0eHw==\" encryptedVerifierHashInput=\"Bqhy/Qd2OTkFJ2d107wA9w==\" encryptedVerifierHashValue=\"UZeG01CrsOKK0lI7LAb1QpiEA426VAfvmOcx9nrRSmk=\" encryptedKeyValue=\"HHfBGFlvHJubZ4Ri0n+Wgw==\"/>\
</keyEncryptor></keyEncryptors></encryption>";

/// The 2013 profile's descriptor.
const DESCRIPTOR_2013: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\r\n\
<encryption xmlns=\"http://schemas.microsoft.com/office/2006/encryption\" xmlns:p=\"http://schemas.microsoft.com/office/2006/keyEncryptor/password\">\
<keyData saltSize=\"16\" blockSize=\"16\" keyBits=\"256\" hashSize=\"64\" cipherAlgorithm=\"AES\" cipherChaining=\"ChainingModeCBC\" hashAlgorithm=\"SHA512\" saltValue=\"AAECAwQFBgcICQoLDA0ODw==\"/>\
<keyEncryptors><keyEncryptor uri=\"http://schemas.microsoft.com/office/2006/keyEncryptor/password\">\
<p:encryptedKey spinCount=\"100000\" saltSize=\"16\" blockSize=\"16\" keyBits=\"256\" hashSize=\"64\" cipherAlgorithm=\"AES\" cipherChaining=\"ChainingModeCBC\" hashAlgorithm=\"SHA512\" saltValue=\"EBESExQVFhcYGRobHB0eHw==\" encryptedVerifierHashInput=\"/GtwF3N2etQf9oRa6IiEzg==\" encryptedVerifierHashValue=\"cBxdu7NEcote8fW7SyMp53EeG8bIsq33x4jsxj6Rmkf6Pwcal7yXcztYuqRHCv0/PAevr5BK45xIBHZ/9m9/JA==\" encryptedKeyValue=\"HVsVjOX9xnH2WVlofa9MCq9w1F6M44UMI4sdDd2Pj+M=\"/>\
</keyEncryptor></keyEncryptors></encryption>";

/// The wrapped agile values and the descriptor match the Java fixtures.
#[test]
fn agile_2010_matches_the_java_fixture() {
    let config = AgileEncryptionConfig::office_2010();
    let w = writer(config.clone());
    assert_eq!(
        hex::encode(w.encrypt_verifier_hash_input().unwrap()),
        "06a872fd0776393905276775d3bc00f7"
    );
    assert_eq!(
        hex::encode(w.encrypt_verifier_hash_value().unwrap()),
        "519786d350abb0e28ad2523b2c06f5429884038dba5407ef98e731f67ad14a69"
    );
    assert_eq!(
        hex::encode(w.encrypt_key_value().unwrap()),
        "1c77c118596f1c9b9b678462d27f9683"
    );
    assert_eq!(
        w.unwrap_key_value(&w.encrypt_key_value().unwrap()).unwrap(),
        fixed_material(&config).key_value
    );
    assert_eq!(
        w.unwrap_verifier_hash_input(&w.encrypt_verifier_hash_input().unwrap())
            .unwrap(),
        fixed_material(&config).verifier_input
    );
    assert_eq!(
        String::from_utf8(w.create_descriptor().unwrap()).unwrap(),
        DESCRIPTOR_2010
    );
}

/// The stronger 2013 profile (256 bit key, SHA-512) matches its fixture.
#[test]
fn agile_2013_matches_the_java_fixture() {
    let config = AgileEncryptionConfig::office_2013();
    let w = writer(config.clone());
    assert_eq!(
        hex::encode(w.encrypt_verifier_hash_input().unwrap()),
        "fc6b701773767ad41ff6845ae88884ce"
    );
    assert_eq!(
        hex::encode(w.encrypt_verifier_hash_value().unwrap()),
        "701c5dbbb344728b5ef1f5bb4b2329e7711e1bc6c8b2adf7c788ecc63e919a47fa3f071a97bc97733b58baa4470afd3f3c07afaf904ae39c4804767ff66f7f24"
    );
    assert_eq!(
        hex::encode(w.encrypt_key_value().unwrap()),
        "1d5b158ce5fdc671f65959687daf4c0aaf70d45e8ce3850c238b1d0ddd8f8fe3"
    );
    assert_eq!(
        w.unwrap_key_value(&w.encrypt_key_value().unwrap()).unwrap(),
        fixed_material(&config).key_value
    );
    assert_eq!(
        String::from_utf8(w.create_descriptor().unwrap()).unwrap(),
        DESCRIPTOR_2013
    );
}

/// The page transform for the synthetic page and page 1 block bytes matches
/// the Java `AgileEncryptionWriter.encryptPage` output.
#[test]
fn agile_page_cipher_matches_the_java_fixture() {
    let mut plain = vec![0u8; 4096];
    for (i, b) in plain.iter_mut().enumerate() {
        *b = (i.wrapping_mul(31).wrapping_add(5)) as u8;
    }
    let block_bytes = apply_page_number(&0x1234_abcdu32.to_le_bytes(), 0, 1);

    for (config, first, last) in [
        (
            AgileEncryptionConfig::office_2010(),
            "218fb8abae78d8185c952a5fa792a7aa",
            "9f11ceeeffa78d0ffb5595c4abc1fa2b",
        ),
        (
            AgileEncryptionConfig::office_2013(),
            "de9e8dd2470bb73e3b5dbcf94416511d",
            "9e36a8a59bccaedbd0c2113640a12701",
        ),
    ] {
        let material = fixed_material(&config);
        let mut page = plain.clone();
        AgileEncryptionWriter::encrypt_page(
            &config,
            &material.key_value,
            &material.key_data_salt,
            &block_bytes,
            &mut page,
        )
        .unwrap();
        assert_eq!(hex::encode(&page[..16]), first);
        assert_eq!(hex::encode(&page[4080..]), last);

        // and the read path reverses it
        let w = writer(config.clone());
        let info = {
            let mut info = Vec::new();
            info.extend_from_slice(&0x40u32.to_le_bytes());
            info.extend_from_slice(&w.create_descriptor().unwrap());
            info
        };
        let mut buf = ByteBuf::wrap_le(info);
        let provider =
            AgileEncryptionProvider::new(&mut buf, &get_password_bytes(Some(PASSWORD))).unwrap();
        let mut decrypted = page.clone();
        provider
            .decrypt_page(&0x1234_abcdu32.to_le_bytes(), 1, &mut decrypted)
            .unwrap();
        assert_eq!(decrypted, plain, "the provider reverses encryptPage");
    }
}

/// The agile provider accepts the right password, refuses a wrong one, and
/// recovers the content key from the descriptor.
#[test]
fn agile_provider_verifies_the_password() {
    for config in [
        AgileEncryptionConfig::office_2010(),
        AgileEncryptionConfig::office_2013(),
    ] {
        let w = writer(config);
        let mut info = Vec::new();
        info.extend_from_slice(&0x40u32.to_le_bytes());
        info.extend_from_slice(&w.create_descriptor().unwrap());

        let pwd = get_password_bytes(Some(PASSWORD));
        let mut buf = ByteBuf::wrap_le(info.clone());
        let provider = AgileEncryptionProvider::new(&mut buf, &pwd).unwrap();
        assert!(provider.verify_password(&pwd).unwrap());
        assert!(!provider
            .verify_password(&get_password_bytes(Some("wrong")))
            .unwrap());
        assert_eq!(provider.key_value(), w.key_value());

        // a different reserved value is refused
        let mut bad = info;
        bad[0] = 0x41;
        let mut buf = ByteBuf::wrap_le(bad);
        let err = AgileEncryptionProvider::new(&mut buf, &pwd);
        assert!(err.is_err());
    }
}

/// The password is truncated to 255 characters (`MAX_PASSWORD_LEN`) before it
/// is encoded, and encoded UTF-16LE.
#[test]
fn password_bytes_match_the_java_rule() {
    assert_eq!(
        get_password_bytes(Some("Pa55")),
        vec![0x50, 0x00, 0x61, 0x00, 0x35, 0x00, 0x35, 0x00]
    );
    assert_eq!(get_password_bytes(None), Vec::<u8>::new());
    let long = "x".repeat(300);
    let encoded = get_password_bytes(Some(&long));
    assert_eq!(encoded.len(), 255 * 2);
}

// ---------------------------------------------------------------------------
// ECMA standard / RC4 key derivations
// ---------------------------------------------------------------------------

const SALT: u8 = 0x40;

fn block_bytes() -> Vec<u8> {
    apply_page_number(&0x1234_abcdu32.to_le_bytes(), 0, 1)
}

/// `ECMAStandardEncryptionProvider.cryptDeriveKey` matches the Java fixture.
#[test]
fn standard_key_derivation_matches_the_java_fixture() {
    let pwd = get_password_bytes(Some(PASSWORD));
    let salt = seq(SALT, 16);
    let base_hash = ecma_standard_base_hash(&pwd, &salt);
    assert_eq!(
        hex::encode(&base_hash),
        "d2b65aa31aa46ab3dbf177c3e1a28edb6dff627f"
    );
    let iter_hash = ecma_standard_iter_hash(&base_hash, HASH_ITERATIONS);
    assert_eq!(
        hex::encode(&iter_hash),
        "d0651466b70a5dc51bf4b95194fd444eb646ebb9"
    );
    assert_eq!(
        hex::encode(ecma_standard_derive_key(&iter_hash, &block_bytes(), 16)),
        "9a632ec5fe52865aeaecfbe6bc29b647"
    );
    assert_eq!(
        hex::encode(ecma_standard_derive_key(&iter_hash, &[0u8; 4], 16)),
        "0d067c42733186c4384c8e4609e6dd31"
    );
}

/// `RC4CryptoAPIProvider.computeEncryptionKey` matches the Java fixture.
#[test]
fn rc4_cryptoapi_key_derivation_matches_the_java_fixture() {
    let pwd = get_password_bytes(Some(PASSWORD));
    let salt = seq(SALT, 16);
    let base_hash = ecma_standard_base_hash(&pwd, &salt);
    assert_eq!(
        hex::encode(rc4_cryptoapi_derive_key(
            &base_hash,
            &block_bytes(),
            16,
            128
        )),
        "c43910a33437b3e5a3cdfa7205b8c2c2"
    );
    assert_eq!(
        hex::encode(rc4_cryptoapi_derive_key(&base_hash, &[0u8; 4], 16, 128)),
        "4fc7f0d729072876310c1e0ac2f7b6b2"
    );
    // a 40 bit key is 5 bytes padded out to 16
    let key40 = rc4_cryptoapi_derive_key(&base_hash, &[0u8; 4], 5, 40);
    assert_eq!(key40.len(), 16);
    assert_eq!(&key40[5..], &[0u8; 11]);
}

/// `OfficeBinaryDocRC4Provider`'s fill hash / base hash / page key match the
/// Java fixture.
#[test]
fn binary_rc4_key_derivation_matches_the_java_fixture() {
    let pwd = get_password_bytes(Some(PASSWORD));
    let salt = seq(SALT, 16);
    assert_eq!(
        hex::encode(binary_rc4_fill_hash(&pwd, &salt)),
        "0355e4d759404142434445464748494a4b4c4d4e4f"
    );
    let base_hash = binary_rc4_base_hash(&pwd, &salt);
    assert_eq!(hex::encode(&base_hash), "b567ee3b3e");
    assert_eq!(
        hex::encode(binary_rc4_derive_key(&base_hash, &block_bytes())),
        "6eb9d0de5e796f7f673e74f478f4428b"
    );
    assert_eq!(
        hex::encode(binary_rc4_derive_key(&base_hash, &[0u8; 4])),
        "4cb68489f08737a1f38b7ae204db25be"
    );
}

// ---------------------------------------------------------------------------
// EncryptionHeader / EncryptionVerifier
// ---------------------------------------------------------------------------

fn put_int(b: &mut [u8], off: usize, v: i32) {
    b[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// The OC 2.3.2 header the Java writer builds, with a null terminated UTF-16LE
/// CSP name.
fn header_bytes(flags: i32, alg_id: i32, key_size: i32, provider_type: i32, csp: &str) -> Vec<u8> {
    let mut name: Vec<u8> = Vec::new();
    for u in csp.encode_utf16() {
        name.extend_from_slice(&u.to_le_bytes());
    }
    name.extend_from_slice(&0u16.to_le_bytes());
    let mut header = vec![0u8; 32 + name.len()];
    put_int(&mut header, 0, flags);
    put_int(&mut header, 4, 0);
    put_int(&mut header, 8, alg_id);
    put_int(&mut header, 12, 0x8004);
    put_int(&mut header, 16, key_size);
    put_int(&mut header, 20, provider_type);
    put_int(&mut header, 24, 0);
    put_int(&mut header, 28, 0);
    header[32..].copy_from_slice(&name);
    header
}

fn aes_ecb_encrypt(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut cipher =
        BlockCipherState::new(CipherAlgorithm::Aes, CipherChaining::Ecb, key, None).unwrap();
    let mut out = data.to_vec();
    cipher.encrypt(&mut out).unwrap();
    out
}

/// Builds the buffer an `ECMAStandardEncryptionProvider` constructor sees:
/// flags, header length, header, verifier.
fn standard_info(flags: i32, alg_id: i32, key_size: i32, csp: &str) -> Vec<u8> {
    let pwd = get_password_bytes(Some(PASSWORD));
    let salt = seq(SALT, 16);
    let verifier = seq(0x50, 16);
    let base_hash = ecma_standard_base_hash(&pwd, &salt);
    let iter_hash = ecma_standard_iter_hash(&base_hash, HASH_ITERATIONS);
    let key0 = ecma_standard_derive_key(&iter_hash, &[0u8; 4], 16);
    let enc_verifier = aes_ecb_encrypt(&key0, &verifier);
    let verifier_hash = fix_to_length(&HashAlgorithm::Sha1.hash(&verifier), 32);
    let enc_verifier_hash = aes_ecb_encrypt(&key0, &verifier_hash);

    let header = header_bytes(flags, alg_id, key_size, 0x18, csp);
    let mut info = Vec::new();
    info.extend_from_slice(&(header.len() as u32).to_le_bytes());
    info.extend_from_slice(&header);
    info.extend_from_slice(&16u32.to_le_bytes());
    info.extend_from_slice(&salt);
    info.extend_from_slice(&enc_verifier);
    info.extend_from_slice(&20u32.to_le_bytes());
    info.extend_from_slice(&enc_verifier_hash);
    info
}

/// The standard provider parses the header and verifier and verifies the
/// password the Java writer wrapped, and refuses a wrong one.
#[test]
fn standard_provider_parses_the_header_and_verifies_the_password() {
    let info = standard_info(
        0x24,
        0x660E,
        128,
        "Microsoft Enhanced RSA and AES Cryptographic Provider",
    );
    let mut buf = ByteBuf::wrap_le(info.clone());
    let provider = EcmaStandardProvider::new(
        &mut buf,
        &get_password_bytes(Some(PASSWORD)),
        HASH_ITERATIONS,
    )
    .unwrap();
    assert!(provider.verify_password().unwrap());
    let header = provider.header();
    assert_eq!(
        header.crypto_algorithm,
        super::header::CryptoAlgorithm::Aes128
    );
    assert_eq!(
        header.hash_algorithm,
        super::header::HeaderHashAlgorithm::Sha1
    );
    assert_eq!(header.key_size, 128);
    assert_eq!(
        header.csp_name,
        "Microsoft Enhanced RSA and AES Cryptographic Provider"
    );

    // the verifier of a different password does not match
    let mut buf = ByteBuf::wrap_le(info);
    let provider = EcmaStandardProvider::new(
        &mut buf,
        &get_password_bytes(Some("wrong")),
        HASH_ITERATIONS,
    )
    .unwrap();
    assert!(!provider.verify_password().unwrap());
}

/// `EncryptionHeader.parseKeySize`: a zero key size takes the RC4 provider
/// default (40 bits for the Base CSP, 128 otherwise) and the algorithm minimum
/// for the AES ids.
#[test]
fn header_key_size_defaults_match_the_java_rule() {
    for (key_size, csp, expected) in [
        (0, "Microsoft Base Cryptographic Provider v1.0", 40),
        (0, "Microsoft Strong Cryptographic Provider", 128),
        (0, "", 40),
        (128, "anything", 128),
    ] {
        let header = header_bytes(0x04, 0x6801, key_size, 1, csp);
        let mut info = (header.len() as u32).to_le_bytes().to_vec();
        info.extend_from_slice(&header);
        let mut buf = ByteBuf::wrap_le(info);
        let header = super::header::EncryptionHeader::read(
            &mut buf,
            &[super::header::CryptoAlgorithm::Rc4],
            &[super::header::HeaderHashAlgorithm::Sha1],
        )
        .unwrap();
        assert_eq!(header.key_size, expected, "csp {:?}", csp);
    }

    // AES with a zero key size uses the algorithm minimum
    let header = header_bytes(0x24, 0x660E, 0, 0x18, "csp");
    let mut info = (header.len() as u32).to_le_bytes().to_vec();
    info.extend_from_slice(&header);
    let mut buf = ByteBuf::wrap_le(info);
    let header = super::header::EncryptionHeader::read(
        &mut buf,
        &[super::header::CryptoAlgorithm::Aes128],
        &[super::header::HeaderHashAlgorithm::Sha1],
    )
    .unwrap();
    assert_eq!(header.key_size, 128);
}

/// The wrapped values use the block constants the OC spec names.
#[test]
fn agile_block_constants_are_the_spec_values() {
    assert_eq!(
        VERIFIER_INPUT_BLOCK,
        [0xfe, 0xa7, 0xd2, 0x76, 0x3b, 0x4b, 0x9e, 0x79]
    );
    assert_eq!(
        VERIFIER_VALUE_BLOCK,
        [0xd7, 0xaa, 0x0f, 0x6d, 0x30, 0x61, 0x34, 0x4e]
    );
    assert_eq!(
        KEY_VALUE_BLOCK,
        [0x14, 0x6e, 0x0b, 0xe7, 0xab, 0xac, 0xd0, 0xd6]
    );
}

// ---------------------------------------------------------------------------
// a self contained round trip (no Java): create, encrypt, open with the
// password, decrypt back to the original bytes
// ---------------------------------------------------------------------------

#[test]
fn office_encryption_round_trip() {
    use crate::column::ColumnSpec;
    use crate::database::Database;
    use crate::datatype::DataType;
    use crate::format::FileFormat;
    use crate::tablecreator::{IndexSpec, TableSpec};
    use crate::value::Value;

    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp");
    let _ = std::fs::create_dir_all(&dir);
    let base = dir.join("office_rt_base.accdb");
    let encrypted = dir.join("office_rt_enc.accdb");
    let encrypted2013 = dir.join("office_rt_enc13.accdb");
    let decrypted = dir.join("office_rt_dec.accdb");
    let decrypted2013 = dir.join("office_rt_dec13.accdb");
    for p in [
        &base,
        &encrypted,
        &encrypted2013,
        &decrypted,
        &decrypted2013,
    ] {
        let _ = std::fs::remove_file(p);
    }

    {
        let mut db = Database::create(FileFormat::V2016, &base).unwrap();
        let spec = TableSpec::new(
            "T",
            vec![
                ColumnSpec::new("ID", DataType::Long).with_auto_number(true),
                ColumnSpec::new("Name", DataType::Text).with_length(20),
            ],
        )
        .with_index(IndexSpec::primary_key(
            "PrimaryKey",
            vec![("ID".to_string(), true)],
        ));
        db.create_table(&spec).unwrap();
        for i in 0..3 {
            db.add_rows("T", &[vec![Value::Null, Value::Text(format!("n{}", i))]])
                .unwrap();
        }
        db.flush().unwrap();
    }
    let base_bytes = std::fs::read(&base).unwrap();

    // the two profiles, with a small spin count: the full 100000 iteration
    // path is pinned by the Java fixture tests and the external harness, this
    // test is about the container round trip
    let config2010 = AgileEncryptionConfig::new(
        CipherAlgorithm::Aes,
        CipherChaining::Cbc,
        128,
        HashAlgorithm::Sha1,
        100,
        16,
    )
    .unwrap();
    let config2013 = AgileEncryptionConfig::new(
        CipherAlgorithm::Aes,
        CipherChaining::Cbc,
        256,
        HashAlgorithm::Sha512,
        100,
        16,
    )
    .unwrap();
    for (path, config) in [(&encrypted, config2010), (&encrypted2013, config2013)] {
        crate::crypt::add_office_encryption_with_config(&base, path, "pw-Test1", &config).unwrap();

        // the encrypted file needs the password; the default provider refuses it
        assert!(Database::open_path(path).is_err());
        match Database::open_path_with_password(path, Some("wrong")) {
            Err(crate::Error::InvalidCredentials(message)) => {
                assert_eq!(message, "Incorrect password provided")
            }
            Err(other) => panic!("expected invalid credentials, got {}", other),
            Ok(_) => panic!("expected invalid credentials, the file opened"),
        }

        {
            let mut db = Database::open_path_with_password(path, Some("pw-Test1")).unwrap();
            let rows = db.read_rows_with_ids("T").unwrap();
            assert_eq!(rows.len(), 3);
            assert_eq!(
                rows[0].1.as_ref().unwrap()[1],
                Value::Text("n0".to_string())
            );
        }

        // a plain .accdb cannot be removed twice, and the decrypted bytes are
        // the original database again
        if path == &encrypted {
            crate::crypt::remove_office_encryption(path, &decrypted, "pw-Test1").unwrap();
            assert_eq!(std::fs::read(&decrypted).unwrap(), base_bytes);
            assert!(crate::crypt::remove_office_encryption(
                &decrypted,
                &dir.join("office_rt_double.accdb"),
                "pw-Test1"
            )
            .is_err());
        } else {
            crate::crypt::remove_office_encryption(path, &decrypted2013, "pw-Test1").unwrap();
            assert_eq!(std::fs::read(&decrypted2013).unwrap(), base_bytes);
        }

        // and the plain database cannot be encrypted twice
        assert!(crate::crypt::add_office_encryption_with_config(
            path,
            &dir.join("office_rt_double_enc.accdb"),
            "pw-Test1",
            &config
        )
        .is_err());
    }

    for p in [
        &base,
        &encrypted,
        &encrypted2013,
        &decrypted,
        &decrypted2013,
    ] {
        let _ = std::fs::remove_file(p);
    }
}
