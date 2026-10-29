//! Legacy AirPlay pairing: /pair-setup hands out our Ed25519 key, /pair-verify
//! runs an X25519 exchange whose shared secret later salts the stream keys.

use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha512};
use x25519_dalek::{PublicKey, StaticSecret};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

pub struct Identity {
    key: SigningKey,
}

impl Identity {
    pub fn generate() -> Self {
        Identity { key: SigningKey::from_bytes(&random_bytes()) }
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
}

pub struct Verify {
    ours: [u8; 32],
    theirs: [u8; 32],
    their_identity: [u8; 32],
    pub shared: [u8; 32],
}

/// First /pair-verify step. Returns our ephemeral key + encrypted signature.
pub fn verify_start(identity: &Identity, body: &[u8]) -> Option<(Vec<u8>, Verify)> {
    if body.len() != 4 + 32 + 32 {
        return None;
    }
    let theirs: [u8; 32] = body[4..36].try_into().ok()?;
    let their_identity: [u8; 32] = body[36..68].try_into().ok()?;

    let secret = StaticSecret::from(random_bytes());
    let ours = PublicKey::from(&secret).to_bytes();
    let shared = secret.diffie_hellman(&PublicKey::from(theirs)).to_bytes();

    let mut signature = identity.key.sign(&[ours, theirs].concat()).to_bytes();
    session_cipher(&shared).apply_keystream(&mut signature);

    let response = [ours.as_slice(), signature.as_slice()].concat();
    Some((response, Verify { ours, theirs, their_identity, shared }))
}

/// Second /pair-verify step: check the client's signature over both ephemeral keys.
pub fn verify_finish(verify: &Verify, body: &[u8]) -> bool {
    let Some(encrypted) = body.get(4..68) else { return false };
    let mut cipher = session_cipher(&verify.shared);
    // The keystream continues where our own signature left off.
    cipher.apply_keystream(&mut [0u8; 64]);
    let mut signature = [0u8; 64];
    signature.copy_from_slice(encrypted);
    cipher.apply_keystream(&mut signature);

    let message = [verify.theirs, verify.ours].concat();
    VerifyingKey::from_bytes(&verify.their_identity)
        .and_then(|key| key.verify(&message, &Signature::from_bytes(&signature)))
        .is_ok()
}

fn session_cipher(shared: &[u8; 32]) -> Aes128Ctr {
    let key = derive(b"Pair-Verify-AES-Key", shared);
    let iv = derive(b"Pair-Verify-AES-IV", shared);
    Aes128Ctr::new(&key.into(), &iv.into())
}

fn derive(salt: &[u8], secret: &[u8]) -> [u8; 16] {
    let hash = Sha512::new().chain_update(salt).chain_update(secret).finalize();
    hash[..16].try_into().unwrap()
}

/// The client's FairPlay key is re-hashed with the pairing secret when pairing ran.
pub fn salt_aes_key(aeskey: [u8; 16], shared: Option<&[u8; 32]>) -> [u8; 16] {
    match shared {
        Some(shared) => derive(&aeskey, shared),
        None => aeskey,
    }
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes).expect("OS random number generator unavailable");
    bytes
}
