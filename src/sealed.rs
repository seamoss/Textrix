//! Small files only the keychain can open: what Textrix keeps for later
//! (messages you've scheduled), encrypted like the store, with a key of
//! their own that the store's keychain secret unlocks.

use std::path::Path;

use anyhow::{Context, Result};
use matrix_sdk_store_encryption::{EncryptedValueBase64, StoreCipher};

pub struct Sealer {
    cipher: StoreCipher,
}

impl Sealer {
    /// The sealer in `dir`, unlocked with `key`. A new one if `key` can't
    /// open the old (after a new login), which forgets what the old one
    /// sealed.
    pub fn open(dir: &Path, key: &[u8]) -> Result<Self> {
        let key_file = dir.join("seal.key");
        let cipher = match std::fs::read(&key_file).ok().and_then(|b| StoreCipher::import_with_key(key, &b).ok()) {
            Some(cipher) => cipher,
            None => {
                let cipher = StoreCipher::new().context("creating a key for sealed files")?;
                crate::config::write_private(&key_file, &cipher.export_with_key(key)?)?;
                cipher
            }
        };
        Ok(Self { cipher })
    }

    /// Write `data` to `path`, sealed; only you can read the file.
    pub fn write(&self, path: &Path, data: &[u8]) -> Result<()> {
        let sealed = self.cipher.encrypt_value_base64_data(data.to_vec())?;
        crate::config::write_private(path, &serde_json::to_vec(&sealed)?)
    }

    /// What's sealed in `path`, if anything this sealer can open.
    pub fn read(&self, path: &Path) -> Option<Vec<u8>> {
        let sealed: EncryptedValueBase64 = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
        self.cipher.decrypt_value_base64_data(sealed).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_files_open_only_with_the_key() {
        let dir = std::env::temp_dir().join(format!("textrix-sealed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("later");
        let sealer = Sealer::open(&dir, b"the keychain secret").unwrap();
        sealer.write(&path, b"lunch at 1?").unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("lunch"), "nothing in the clear");
        assert_eq!(Sealer::open(&dir, b"the keychain secret").unwrap().read(&path).unwrap(), b"lunch at 1?");
        assert!(Sealer::open(&dir, b"another secret").unwrap().read(&path).is_none(), "a new key can't");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
