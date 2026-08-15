//! SCRAM-SHA-256 client (RFC 5802 / RFC 7677) — Postgres's default password
//! authentication. Channel binding is not offered (`n,,` GS2 header) because
//! connections are plain TCP.

use super::b64;
use crate::crypto::{hmac_sha256, pbkdf2_hmac_sha256, sha256};
use crate::{rerr, Result};

pub struct ScramClient {
    password: String,
    client_nonce: String,
    client_first_bare: String,
    /// Set after the server-first message is processed.
    auth_message: Option<String>,
    server_signature: Option<[u8; 32]>,
}

impl ScramClient {
    pub fn new(password: &str, entropy: &[u8]) -> ScramClient {
        // Nonce: base64 of a digest over caller-supplied entropy. Uniqueness
        // is what matters for correctness; entropy comes from the OS when
        // available (see proto.rs).
        let nonce = b64::encode(&sha256(entropy))[..24].to_string();
        let client_first_bare = format!("n=,r={nonce}");
        ScramClient {
            password: password.to_string(),
            client_nonce: nonce,
            client_first_bare,
            auth_message: None,
            server_signature: None,
        }
    }

    /// The SASL initial response payload.
    pub fn client_first(&self) -> String {
        format!("n,,{}", self.client_first_bare)
    }

    /// Process server-first, produce client-final.
    pub fn client_final(&mut self, server_first: &str) -> Result<String> {
        let mut nonce = None;
        let mut salt = None;
        let mut iterations = None;
        for part in server_first.split(',') {
            match part.split_once('=') {
                Some(("r", v)) => nonce = Some(v.to_string()),
                Some(("s", v)) => salt = Some(v.to_string()),
                Some(("i", v)) => iterations = v.parse::<u32>().ok(),
                _ => {}
            }
        }
        let nonce = nonce.ok_or_else(|| rerr("SCRAM: server-first missing nonce"))?;
        let salt = salt.ok_or_else(|| rerr("SCRAM: server-first missing salt"))?;
        let iterations = iterations.ok_or_else(|| rerr("SCRAM: server-first missing iterations"))?;
        if !nonce.starts_with(&self.client_nonce) {
            return Err(rerr("SCRAM: server nonce does not extend client nonce"));
        }
        if !(1..=10_000_000).contains(&iterations) {
            return Err(rerr(format!("SCRAM: unreasonable iteration count {iterations}")));
        }
        let salt_bytes =
            b64::decode(&salt).ok_or_else(|| rerr("SCRAM: server salt is not valid base64"))?;

        let salted = pbkdf2_hmac_sha256(self.password.as_bytes(), &salt_bytes, iterations);
        let client_key = hmac_sha256(&salted, b"Client Key");
        let stored_key = sha256(&client_key);
        let server_key = hmac_sha256(&salted, b"Server Key");

        let client_final_without_proof = format!("c={},r={nonce}", b64::encode(b"n,,"));
        let auth_message =
            format!("{},{server_first},{client_final_without_proof}", self.client_first_bare);
        let client_signature = hmac_sha256(&stored_key, auth_message.as_bytes());
        let mut proof = client_key;
        for i in 0..32 {
            proof[i] ^= client_signature[i];
        }
        self.server_signature = Some(hmac_sha256(&server_key, auth_message.as_bytes()));
        self.auth_message = Some(auth_message);
        Ok(format!("{client_final_without_proof},p={}", b64::encode(&proof)))
    }

    /// Verify the server-final message (v=...) proves the server knew the
    /// password derivative too.
    pub fn verify_server_final(&self, server_final: &str) -> Result<()> {
        let expected = self
            .server_signature
            .ok_or_else(|| rerr("SCRAM: server-final before client-final"))?;
        for part in server_final.split(',') {
            if let Some(("v", v)) = part.split_once('=') {
                let sig = b64::decode(v)
                    .ok_or_else(|| rerr("SCRAM: server signature is not valid base64"))?;
                if sig == expected {
                    return Ok(());
                }
                return Err(rerr("SCRAM: server signature mismatch — server does not know the password"));
            }
            if let Some(("e", msg)) = part.split_once('=') {
                return Err(rerr(format!("SCRAM: server error: {msg}")));
            }
        }
        Err(rerr("SCRAM: server-final missing verifier"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7677 §3 full exchange, with the client nonce pinned to the RFC's.
    #[test]
    fn rfc7677_exchange() {
        let mut c = ScramClient::new("pencil", b"ignored");
        // Pin the nonce to the RFC value.
        c.client_nonce = "rOprNGfwEbeRWgbNEkqO".to_string();
        c.client_first_bare = format!("n=,r={}", c.client_nonce);
        // (RFC uses n=user; Postgres sends the username via startup, n= empty.
        // To match the RFC transcript exactly we emulate its bare header.)
        c.client_first_bare = "n=user,r=rOprNGfwEbeRWgbNEkqO".to_string();

        let server_first =
            "r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        let client_final = c.client_final(server_first).unwrap();
        assert_eq!(
            client_final,
            "c=biws,r=rOprNGfwEbeRWgbNEkqO%hvYDpWUa2RaTCAfuxFIlj)hNlF$k0,p=dHzbZapWIk4jUhN+Ute9ytag9zjfMHgsqmmiz7AndVQ="
        );
        c.verify_server_final("v=6rriTRBi23WpRR/wtup+mMhUZUn/dB5nLTJRsjl95G4=").unwrap();
        assert!(c.verify_server_final("v=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=").is_err());
    }

    #[test]
    fn rejects_tampered_nonce() {
        let mut c = ScramClient::new("pw", b"e");
        let bogus = "r=completely-different,s=W22ZaJ0SNY7soEsUEjb6gQ==,i=4096";
        assert!(c.client_final(bogus).is_err());
    }
}
