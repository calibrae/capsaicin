//! SPICE ticket encryption: RSA-1024 with OAEP/SHA-1 padding.

use capsaicin_proto::enums::{SPICE_MAX_PASSWORD_LENGTH, SPICE_TICKET_PUBKEY_BYTES};
use rand::rngs::OsRng;
use rsa::{Oaep, RsaPublicKey, pkcs8::DecodePublicKey};
use sha1::Sha1;

use crate::{NetError, Result};

/// RSA-OAEP-SHA1 encrypt a password with the server-provided 1024-bit public
/// key. The plaintext includes a trailing `\0` byte, matching spice-gtk.
/// Output is always 128 bytes (RSA-1024 block size).
pub fn encrypt_ticket(
    pub_key_der: &[u8; SPICE_TICKET_PUBKEY_BYTES],
    password: &str,
) -> Result<Vec<u8>> {
    if password.len() > SPICE_MAX_PASSWORD_LENGTH {
        return Err(NetError::PasswordTooLong {
            len: password.len(),
        });
    }

    let key = RsaPublicKey::from_public_key_der(pub_key_der).map_err(|_| NetError::BadServerKey)?;

    let mut plaintext = Vec::with_capacity(password.len() + 1);
    plaintext.extend_from_slice(password.as_bytes());
    plaintext.push(0);

    key.encrypt(&mut OsRng, Oaep::new::<Sha1>(), &plaintext)
        .map_err(|e| NetError::RsaEncrypt(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::{RsaPrivateKey, pkcs8::EncodePublicKey};

    #[test]
    fn roundtrip_matches_spice_gtk_framing() {
        let mut rng = OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 1024).unwrap();
        let pub_key = RsaPublicKey::from(&priv_key);

        let der = pub_key.to_public_key_der().unwrap();
        assert_eq!(der.as_bytes().len(), SPICE_TICKET_PUBKEY_BYTES);

        let mut fixed = [0u8; SPICE_TICKET_PUBKEY_BYTES];
        fixed.copy_from_slice(der.as_bytes());

        let ct = encrypt_ticket(&fixed, "hunter2").unwrap();
        assert_eq!(ct.len(), 128, "RSA-1024 output must be 128 bytes");

        let pt = priv_key.decrypt(Oaep::new::<Sha1>(), &ct).unwrap();
        assert_eq!(&pt[..pt.len() - 1], b"hunter2");
        assert_eq!(pt.last().copied(), Some(0));
    }

    #[test]
    fn rejects_password_over_max() {
        let pk = [0u8; SPICE_TICKET_PUBKEY_BYTES];
        let long = "a".repeat(SPICE_MAX_PASSWORD_LENGTH + 1);
        assert!(matches!(
            encrypt_ticket(&pk, &long),
            Err(NetError::PasswordTooLong { .. })
        ));
    }
}
