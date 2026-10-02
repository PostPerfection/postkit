use std::path::Path;

use asdcplib::crypto::{AesDecContext, HmacContext};
use serde::Deserialize;
use zeroize::{Zeroize, Zeroizing};

const KEY_ID_URN_PREFIX: &str = "urn:uuid:";
const CONTENT_KEY_BYTES: usize = 16;

struct ContentKeyEntry {
    key_id: uuid::Uuid,
    content_key: [u8; CONTENT_KEY_BYTES],
}

impl Drop for ContentKeyEntry {
    fn drop(&mut self) {
        self.content_key.zeroize();
    }
}

pub struct ContentKeys {
    entries: Vec<ContentKeyEntry>,
}

impl std::fmt::Debug for ContentKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key_ids: Vec<&uuid::Uuid> = self.entries.iter().map(|entry| &entry.key_id).collect();
        f.debug_struct("ContentKeys")
            .field("key_ids", &key_ids)
            .field("content_keys", &"<redacted>")
            .finish()
    }
}

#[derive(Deserialize)]
struct KeysFile {
    keys: Vec<KeysFileEntry>,
}

#[derive(Deserialize)]
struct KeysFileEntry {
    key_id: Option<String>,
    content_key_hex: Option<String>,
}

impl Drop for KeysFileEntry {
    fn drop(&mut self) {
        self.content_key_hex.zeroize();
    }
}

impl ContentKeys {
    pub fn from_kdm(kdm: &Path, recipient_key: &Path) -> Result<Self, String> {
        let unwrapped = crate::certificate::unwrap_kdm_file(kdm, recipient_key)?;
        let mut entries = Vec::with_capacity(unwrapped.keys.len());
        for key in &unwrapped.keys {
            entries.push(ContentKeyEntry {
                key_id: key.key_id,
                content_key: *key.content_key(),
            });
        }
        Ok(ContentKeys { entries })
    }

    pub fn from_keys_json(path: &Path) -> Result<Self, String> {
        let text = Zeroizing::new(
            std::fs::read_to_string(path)
                .map_err(|e| format!("cannot read keys file {}: {e}", path.display()))?,
        );
        let file: KeysFile = serde_json::from_str(&text).map_err(|e| {
            format!(
                "bad keys file {}: {}",
                path.display(),
                json_error_without_values(&e)
            )
        })?;
        let mut keys = ContentKeys {
            entries: Vec::with_capacity(file.keys.len()),
        };
        for (index, entry) in file.keys.iter().enumerate() {
            let named =
                |problem: &str| format!("keys file {} entry {index}: {problem}", path.display());
            let key_id_text = entry.key_id.as_deref().ok_or_else(|| named("no key_id"))?;
            let content_key_hex = entry
                .content_key_hex
                .as_deref()
                .ok_or_else(|| named("no content_key_hex"))?;
            let key_id =
                uuid::Uuid::parse_str(key_id_text.trim_start_matches(KEY_ID_URN_PREFIX))
                    .map_err(|e| named(&format!("key_id '{key_id_text}' is not a UUID: {e}")))?;
            let mut decoded = ContentKeyEntry {
                key_id,
                content_key: [0; CONTENT_KEY_BYTES],
            };
            // the hex crate's error quotes the offending character of the key
            hex::decode_to_slice(content_key_hex, &mut decoded.content_key).map_err(|_| {
                named(&format!(
                    "content_key_hex is not {} hex digits",
                    CONTENT_KEY_BYTES * 2
                ))
            })?;
            // a key id listed twice keeps its last key
            keys.entries.retain(|held| held.key_id != key_id);
            keys.entries.push(decoded);
        }
        Ok(keys)
    }

    pub fn from_options(
        kdm: Option<&Path>,
        recipient_key: Option<&Path>,
        keys: Option<&Path>,
    ) -> Result<Option<Self>, String> {
        if let Some(keys) = keys {
            return Self::from_keys_json(keys).map(Some);
        }
        match (kdm, recipient_key) {
            (Some(kdm), Some(recipient_key)) => Self::from_kdm(kdm, recipient_key).map(Some),
            (None, None) => Ok(None),
            _ => Err("decrypting needs both --kdm and --recipient-key (or use --keys)".into()),
        }
    }

    pub fn content_key(&self, key_id: &uuid::Uuid) -> Option<&[u8; CONTENT_KEY_BYTES]> {
        self.entries
            .iter()
            .find(|entry| &entry.key_id == key_id)
            .map(|entry| &entry.content_key)
    }

    pub(crate) fn covering_key(
        &self,
        info: &asdcplib::WriterInfo,
        what: &str,
    ) -> Result<&[u8; CONTENT_KEY_BYTES], String> {
        let key_id = uuid::Uuid::from_bytes(info.cryptographic_key_id);
        self.content_key(&key_id)
            .ok_or_else(|| format!("KDM/keys do not cover {what} KeyId {key_id}"))
    }

    pub fn decrypt_context(
        &self,
        info: &asdcplib::WriterInfo,
        what: &str,
    ) -> Result<AesDecContext, String> {
        let key = self.covering_key(info, what)?;
        let mut decrypt = AesDecContext::new();
        decrypt
            .init_key(key)
            .map_err(|e| format!("AES key init failed: {e}"))?;
        Ok(decrypt)
    }

    pub fn decrypt_and_hmac_contexts(
        &self,
        info: &asdcplib::WriterInfo,
        what: &str,
    ) -> Result<(AesDecContext, HmacContext), String> {
        let decrypt = self.decrypt_context(info, what)?;
        let key = self.covering_key(info, what)?;
        let mut hmac = HmacContext::new();
        hmac.init_key(key, info.label_set)
            .map_err(|e| format!("HMAC key init failed: {e}"))?;
        Ok((decrypt, hmac))
    }
}

// serde_json type errors quote the value, which can be a content key
fn json_error_without_values(error: &serde_json::Error) -> String {
    let description = match error.classify() {
        serde_json::error::Category::Data => "a value has the wrong type or a field is missing",
        serde_json::error::Category::Syntax => "not valid JSON",
        serde_json::error::Category::Eof => "the file ends inside the JSON",
        serde_json::error::Category::Io => "cannot read the JSON",
    };
    format!(
        "line {} column {}: {description}",
        error.line(),
        error.column()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::certificate::{KdmConfig, KdmContentKey, KdmFormulation, build_kdm, generate_chain};

    const PICTURE_KEY_ID: &str = "8f2c6a10-3b4d-4e5f-8a6b-7c8d9e0f1a2b";
    const SOUND_KEY_ID: &str = "1a2b3c4d-5e6f-4a7b-8c9d-0e1f2a3b4c5d";
    const PICTURE_KEY_HEX: &str = "00112233445566778899aabbccddeeff";
    const SOUND_KEY_HEX: &str = "ffeeddccbbaa99887766554433221100";
    const BAD_KEY_HEX: &str = "zz112233445566778899aabbccddeeff";

    fn uuid(text: &str) -> uuid::Uuid {
        uuid::Uuid::parse_str(text).unwrap()
    }

    fn key_bytes(hex_text: &str) -> [u8; CONTENT_KEY_BYTES] {
        let mut key = [0; CONTENT_KEY_BYTES];
        hex::decode_to_slice(hex_text, &mut key).unwrap();
        key
    }

    fn keys_json(directory: &Path, entries: &[(&str, &str, &str)]) -> std::path::PathBuf {
        let keys: Vec<serde_json::Value> = entries
            .iter()
            .map(|(key_type, key_id, content_key_hex)| {
                serde_json::json!({
                    "key_type": key_type,
                    "key_id": key_id,
                    "asset_uuid": "c0ffee00-0000-4000-8000-000000000000",
                    "content_key_hex": content_key_hex,
                })
            })
            .collect();
        let path = directory.join("KEYS.json");
        let file = serde_json::json!({ "cpl_id": "cpl", "keys": keys });
        std::fs::write(&path, file.to_string()).unwrap();
        path
    }

    #[test]
    fn a_keys_file_loads_every_entry_whatever_its_type() {
        let directory = tempfile::tempdir().unwrap();
        let path = keys_json(
            directory.path(),
            &[
                ("Mdik", PICTURE_KEY_ID, PICTURE_KEY_HEX),
                ("Mdak", &format!("urn:uuid:{SOUND_KEY_ID}"), SOUND_KEY_HEX),
            ],
        );
        let keys = ContentKeys::from_keys_json(&path).unwrap();
        assert_eq!(
            keys.content_key(&uuid(PICTURE_KEY_ID)),
            Some(&key_bytes(PICTURE_KEY_HEX))
        );
        assert_eq!(
            keys.content_key(&uuid(SOUND_KEY_ID)),
            Some(&key_bytes(SOUND_KEY_HEX))
        );
        assert_eq!(keys.content_key(&uuid::Uuid::nil()), None);
    }

    #[test]
    fn a_bad_hex_entry_fails_by_name_without_the_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = keys_json(
            directory.path(),
            &[
                ("Mdik", PICTURE_KEY_ID, PICTURE_KEY_HEX),
                ("Mdak", SOUND_KEY_ID, BAD_KEY_HEX),
            ],
        );
        let error = ContentKeys::from_keys_json(&path).unwrap_err();
        assert!(error.contains("entry 1"), "{error}");
        assert!(error.contains("content_key_hex"), "{error}");
        assert!(!error.contains(&BAD_KEY_HEX[2..]), "{error}");
    }

    #[test]
    fn an_entry_missing_its_key_id_fails_by_name() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("KEYS.json");
        std::fs::write(
            &path,
            format!(r#"{{"keys": [{{"content_key_hex": "{PICTURE_KEY_HEX}"}}]}}"#),
        )
        .unwrap();
        let error = ContentKeys::from_keys_json(&path).unwrap_err();
        assert!(error.contains("entry 0: no key_id"), "{error}");
    }

    #[test]
    fn the_options_take_a_keys_file_first_and_refuse_half_a_kdm() {
        let directory = tempfile::tempdir().unwrap();
        let path = keys_json(
            directory.path(),
            &[("Mdik", PICTURE_KEY_ID, PICTURE_KEY_HEX)],
        );
        let missing = directory.path().join("missing.xml");

        // the kdm paths do not exist, so reaching them would fail
        let keys = ContentKeys::from_options(Some(&missing), Some(&missing), Some(&path))
            .unwrap()
            .expect("a keys file was given");
        assert_eq!(
            keys.content_key(&uuid(PICTURE_KEY_ID)),
            Some(&key_bytes(PICTURE_KEY_HEX))
        );

        assert!(
            ContentKeys::from_options(None, None, None)
                .unwrap()
                .is_none()
        );
        for (kdm, recipient_key) in [
            (Some(missing.as_path()), None),
            (None, Some(missing.as_path())),
        ] {
            assert_eq!(
                ContentKeys::from_options(kdm, recipient_key, None).unwrap_err(),
                "decrypting needs both --kdm and --recipient-key (or use --keys)"
            );
        }
    }

    #[test]
    fn a_kdm_unwraps_with_the_recipient_key() {
        let directory = tempfile::tempdir().unwrap();
        let chain = directory.path().join("chain");
        assert_eq!(generate_chain("Acme", &chain), 0, "chain generation failed");
        let picture_key = key_bytes(PICTURE_KEY_HEX);
        let sound_key = key_bytes(SOUND_KEY_HEX);
        let kdm = build_kdm(&KdmConfig {
            cpl_id: "8a2b1c3d-4e5f-6071-8293-a4b5c6d7e8f9".to_string(),
            content_title: "Test Feature".to_string(),
            recipient_cert_file: chain.join("signer.pem"),
            signer_cert_file: chain.join("root.pem"),
            signer_key_file: chain.join("root.key"),
            valid_from: (chrono::Utc::now() + chrono::Duration::days(1))
                .format("%Y-%m-%dT%H:%M:%S+00:00")
                .to_string(),
            valid_to: "7 days".to_string(),
            formulation: KdmFormulation::DciAny,
            content_keys: vec![
                KdmContentKey {
                    key_type: *b"MDIK",
                    key_id: uuid(PICTURE_KEY_ID),
                    content_key: picture_key,
                },
                KdmContentKey {
                    key_type: *b"MDAK",
                    key_id: uuid(SOUND_KEY_ID),
                    content_key: sound_key,
                },
            ],
            ..Default::default()
        })
        .unwrap();
        let kdm_file = directory.path().join("kdm.xml");
        std::fs::write(&kdm_file, &kdm.xml).unwrap();

        let keys =
            ContentKeys::from_options(Some(&kdm_file), Some(&chain.join("signer.key")), None)
                .unwrap()
                .expect("a kdm and recipient key were given");
        assert_eq!(keys.content_key(&uuid(PICTURE_KEY_ID)), Some(&picture_key));
        assert_eq!(keys.content_key(&uuid(SOUND_KEY_ID)), Some(&sound_key));
        assert!(
            ContentKeys::from_kdm(&kdm_file, &chain.join("root.key")).is_err(),
            "the wrong recipient key has to fail"
        );
    }

    #[test]
    fn debug_output_names_the_key_ids_and_never_a_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = keys_json(
            directory.path(),
            &[("Mdik", PICTURE_KEY_ID, PICTURE_KEY_HEX)],
        );
        let keys = ContentKeys::from_keys_json(&path).unwrap();
        let printed = format!("{keys:?}");
        assert!(printed.contains(PICTURE_KEY_ID), "{printed}");
        assert!(!printed.contains(PICTURE_KEY_HEX), "{printed}");
        assert!(!printed.contains("0, 17, 34"), "{printed}");
    }

    #[test]
    fn a_context_is_built_only_for_a_covered_key_id() {
        let directory = tempfile::tempdir().unwrap();
        let path = keys_json(
            directory.path(),
            &[("Mdik", PICTURE_KEY_ID, PICTURE_KEY_HEX)],
        );
        let keys = ContentKeys::from_keys_json(&path).unwrap();
        let covered = asdcplib::WriterInfo {
            cryptographic_key_id: *uuid(PICTURE_KEY_ID).as_bytes(),
            ..Default::default()
        };
        assert!(keys.decrypt_and_hmac_contexts(&covered, "picture").is_ok());
        let uncovered = asdcplib::WriterInfo {
            cryptographic_key_id: *uuid(SOUND_KEY_ID).as_bytes(),
            ..Default::default()
        };
        assert_eq!(
            keys.decrypt_context(&uncovered, "sound")
                .err()
                .expect("the key id is not covered"),
            format!("KDM/keys do not cover sound KeyId {SOUND_KEY_ID}")
        );
    }
}
