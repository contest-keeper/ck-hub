//! Encrypts the Wi-Fi password of a profile before the Hub keeps it, and decrypts it for an admin.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

const NONCE_BYTES: usize = 12;

/// AES-256-GCM with a key from the environment. Each value gets its own random nonce, and is tied
/// to the profile it was saved for, so it cannot be copied to another profile.
pub struct SecretBox(Aes256Gcm);

impl SecretBox {
    /// # Errors
    /// When the text is not base64 of exactly 32 bytes.
    pub fn from_base64(text: &str) -> Result<Self, String> {
        let key = STANDARD
            .decode(text.trim())
            .map_err(|_| "CK_SECRET_KEY is not base64".to_owned())?;
        Aes256Gcm::new_from_slice(&key)
            .map(Self)
            .map_err(|_| "CK_SECRET_KEY must be 32 bytes (base64)".to_owned())
    }

    /// `None` only when the system has no randomness, in which case nothing is kept.
    pub fn seal(&self, aad: &str, plain: &str) -> Option<String> {
        let mut nonce = [0u8; NONCE_BYTES];
        getrandom::fill(&mut nonce).ok()?;
        let payload = Payload {
            msg: plain.as_bytes(),
            aad: aad.as_bytes(),
        };
        let sealed = self.0.encrypt(&nonce.into(), payload).ok()?;
        Some(STANDARD.encode([nonce.as_slice(), &sealed].concat()))
    }

    /// `None` for a wrong key, a changed value or another profile's aad.
    pub fn open(&self, aad: &str, sealed: &str) -> Option<String> {
        let bytes = STANDARD.decode(sealed).ok()?;
        let (nonce, rest) = bytes.split_at_checked(NONCE_BYTES)?;
        let nonce: [u8; NONCE_BYTES] = nonce.try_into().ok()?;
        let payload = Payload {
            msg: rest,
            aad: aad.as_bytes(),
        };
        String::from_utf8(self.0.decrypt(&nonce.into(), payload).ok()?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret_box() -> SecretBox {
        SecretBox::from_base64(&STANDARD.encode([7u8; 32])).expect("key")
    }

    #[test]
    fn a_sealed_value_opens_only_for_its_own_profile() {
        let b = secret_box();
        let sealed = b.seal("org/p1", "hunter2-hunter2").expect("sealed");
        assert!(!sealed.contains("hunter2"));
        assert_eq!(
            b.open("org/p1", &sealed).as_deref(),
            Some("hunter2-hunter2")
        );
        assert_eq!(b.open("org/p2", &sealed), None);
        assert_eq!(b.open("org/p1", "AAAA"), None);
    }

    #[test]
    fn a_key_of_the_wrong_size_is_refused() {
        assert!(SecretBox::from_base64(&STANDARD.encode([1u8; 16])).is_err());
    }
}
