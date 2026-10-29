//! FairPlay SAP handshake (/fp-setup) and decryption of the session AES key.
//! The key decryption itself is the vendored playfair C code.

use super::fairplay_tables::{FP_HEADER, REPLY_MESSAGE};

unsafe extern "C" {
    fn playfair_decrypt(message3: *mut u8, cipher_text: *mut u8, key_out: *mut u8);
}

/// Response to the 16-byte first fp-setup message.
pub fn setup_reply(request: &[u8]) -> Option<Vec<u8>> {
    if request.len() != 16 || request[4] != 0x03 {
        return None;
    }
    REPLY_MESSAGE.get(request[14] as usize).map(|reply| reply.to_vec())
}

/// Response to the 164-byte second fp-setup message, which we keep for decryption.
pub fn handshake_reply(request: &[u8]) -> Option<(Vec<u8>, [u8; 164])> {
    let keymsg: [u8; 164] = request.try_into().ok()?;
    // Byte 12 selects one of four key tables in playfair; anything else would
    // make the C code index past them.
    if keymsg[4] != 0x03 || keymsg[12] > 3 {
        return None;
    }
    let reply = [FP_HEADER.as_slice(), &keymsg[144..164]].concat();
    Some((reply, keymsg))
}

pub fn decrypt_key(keymsg: &[u8; 164], ekey: &[u8; 72]) -> [u8; 16] {
    let mut keymsg = *keymsg;
    let mut ekey = *ekey;
    let mut key = [0u8; 16];
    // SAFETY: playfair reads 164 bytes of message3 and 72 bytes of cipherText and
    // writes 16 bytes to keyOut; all three buffers are exactly those sizes.
    unsafe { playfair_decrypt(keymsg.as_mut_ptr(), ekey.as_mut_ptr(), key.as_mut_ptr()) };
    key
}
