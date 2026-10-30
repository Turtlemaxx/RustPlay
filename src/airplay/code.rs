//! The optional connection code: while it's on, a device can only start mirroring
//! with the 4-digit code RustPlay shows on screen. iOS asks for it as the AirPlay
//! password and answers with RFC 2617 digest authentication, as in UxPlay's `-pw`
//! mode. Each code is good for one connection, and wrong guesses are rationed.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use md5::{Digest, Md5};
use serde_json::{Value, json};

use super::hub::Hub;
use super::pairing;

const REALM: &str = "raop";
const MAX_ATTEMPTS: u32 = 3;
const CODE_LIFETIME: Duration = Duration::from_secs(120);
/// The pause after MAX_ATTEMPTS wrong codes doubles up to the maximum, which puts
/// guessing a 4-digit code at days, with the window visibly reacting to every round.
const FIRST_LOCKOUT: Duration = Duration::from_secs(5);
const MAX_LOCKOUT: Duration = Duration::from_secs(60);

pub struct CodeGate {
    hub: Arc<Hub>,
    state: Mutex<State>,
}

struct State {
    required: bool,
    pending: Option<Pending>,
    /// Wrong guesses since the last pause or success. This belongs to the gate rather
    /// than to one code, so starting over with a new code can't earn more guesses.
    wrong: u32,
    locked_until: Option<Instant>,
    lockout: Duration,
}

struct Pending {
    code: String,
    device_name: String,
    nonce: String,
    expires: Instant,
}

impl CodeGate {
    pub fn new(hub: Arc<Hub>, required: bool) -> Self {
        let state = State { required, pending: None, wrong: 0, locked_until: None, lockout: Duration::ZERO };
        CodeGate { hub, state: Mutex::new(state) }
    }

    pub fn required(&self) -> bool {
        self.state.lock().unwrap().required
    }

    pub fn set_required(&self, required: bool) {
        let mut state = self.state.lock().unwrap();
        state.required = required;
        state.wrong = 0;
        state.locked_until = None;
        state.lockout = Duration::ZERO;
        if state.pending.take().is_some() {
            self.hide();
        }
    }

    /// The person at the PC dismissed the code; it stops working.
    pub fn cancel(&self) {
        if self.state.lock().unwrap().pending.take().is_some() {
            self.hide();
        }
    }

    /// Decides whether a device may start a session. `Err` holds the
    /// WWW-Authenticate challenge to answer with: the device then asks for the code.
    pub fn admit(&self, method: &str, authorization: Option<&str>, device_name: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        if !state.required {
            return Ok(());
        }
        let now = Instant::now();
        if state.locked_until.is_some_and(|until| now < until) {
            return Err(challenge(&new_nonce()));
        }
        state.locked_until = None;

        // Device IDs are whatever the client claims, so a new one doesn't get a new
        // code: the code on screen stays until it's used, it expires or guesses run out.
        if state.pending.as_ref().is_none_or(|pending| pending.expires <= now) {
            log::info!("{device_name} needs the connection code");
            state.pending = Some(Pending {
                code: new_code(),
                device_name: String::new(),
                nonce: String::new(),
                expires: now + CODE_LIFETIME,
            });
        }
        if authorization.is_none() {
            state.pending.as_mut().expect("set above").device_name = device_name.to_string();
        }

        let mut wrong_answer = false;
        if let Some(authorization) = authorization {
            let pending = state.pending.as_ref().expect("set above");
            // Only an answer to the current challenge counts as a guess.
            if digest_nonce(authorization) == Some(pending.nonce.as_str()) {
                if digest_matches(authorization, method, &pending.nonce, &pending.code) {
                    log::info!("{device_name} entered the connection code");
                    state.pending = None;
                    state.wrong = 0;
                    state.lockout = Duration::ZERO;
                    self.hide();
                    return Ok(());
                }
                wrong_answer = true;
            }
        }

        if wrong_answer {
            state.wrong += 1;
            if state.wrong >= MAX_ATTEMPTS {
                let lockout = (state.lockout * 2).clamp(FIRST_LOCKOUT, MAX_LOCKOUT);
                log::warn!("too many wrong connection codes; pausing for {} s", lockout.as_secs());
                state.wrong = 0;
                state.lockout = lockout;
                state.locked_until = Some(now + lockout);
                state.pending = None;
                self.notify(json!({ "type": "code", "code": null, "wait": lockout.as_secs() }));
                return Err(challenge(&new_nonce()));
            }
        }

        let pending = state.pending.as_mut().expect("set above");
        pending.nonce = new_nonce();
        let reply = challenge(&pending.nonce);
        self.notify(json!({
            "type": "code",
            "code": pending.code,
            "device": pending.device_name,
            "wrong": wrong_answer,
            "expires": pending.expires.saturating_duration_since(now).as_secs(),
        }));
        Err(reply)
    }

    fn hide(&self) {
        self.notify(json!({ "type": "code", "code": null }));
    }

    fn notify(&self, event: Value) {
        self.hub.notify(event);
    }
}

fn new_code() -> String {
    format!("{:04}", u32::from_le_bytes(pairing::random_bytes()) % 10_000)
}

fn new_nonce() -> String {
    hex::encode(pairing::random_bytes::<16>())
}

fn challenge(nonce: &str) -> String {
    format!("Digest realm=\"{REALM}\", nonce=\"{nonce}\"")
}

fn md5_hex(text: &str) -> String {
    hex::encode(Md5::digest(text))
}

/// The `name=value` pairs of a Digest Authorization header; values may be quoted.
fn digest_fields(authorization: &str) -> Option<Vec<(&str, &str)>> {
    let mut rest = authorization.trim().strip_prefix("Digest ")?;
    let mut fields = Vec::new();
    while let Some((name, tail)) = rest.split_once('=') {
        let (value, tail) = match tail.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => tail.split_once(',').unwrap_or((tail, "")),
        };
        fields.push((name.trim(), value.trim()));
        rest = tail.trim_start_matches([',', ' ']);
    }
    Some(fields)
}

fn digest_nonce(authorization: &str) -> Option<&str> {
    digest_fields(authorization)?.into_iter().find(|(name, _)| *name == "nonce").map(|(_, value)| value)
}

fn digest_matches(authorization: &str, method: &str, nonce: &str, password: &str) -> bool {
    let Some(fields) = digest_fields(authorization) else { return false };
    let field = |wanted: &str| fields.iter().find(|(name, _)| *name == wanted).map(|(_, value)| *value);
    let (Some(username), Some(realm), Some(uri), Some(response)) =
        (field("username"), field("realm"), field("uri"), field("response"))
    else {
        return false;
    };
    let ha1 = md5_hex(&format!("{username}:{realm}:{password}"));
    let ha2 = md5_hex(&format!("{method}:{uri}"));
    let expected = match (field("qop"), field("nc"), field("cnonce")) {
        (Some(qop), Some(nc), Some(cnonce)) => md5_hex(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}")),
        _ => md5_hex(&format!("{ha1}:{nonce}:{ha2}")),
    };
    expected.eq_ignore_ascii_case(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> CodeGate {
        CodeGate::new(Arc::new(Hub::new("Test".into(), None, 0.5)), true)
    }

    fn answer(challenge: &str, code: &str) -> String {
        let nonce = digest_nonce(challenge).unwrap();
        let ha1 = md5_hex(&format!("AirPlay:{REALM}:{code}"));
        let ha2 = md5_hex("SETUP:rtsp://10.0.0.2/1");
        let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
        format!(
            "Digest username=\"AirPlay\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"rtsp://10.0.0.2/1\", response=\"{response}\""
        )
    }

    fn current_code(gate: &CodeGate) -> String {
        gate.state.lock().unwrap().pending.as_ref().unwrap().code.clone()
    }

    #[test]
    fn checks_the_rfc_2617_example() {
        let header = "Digest username=\"Mufasa\", realm=\"testrealm@host.com\", \
                      nonce=\"dcd98b7102dd2f0e8b11d0f600bfb0c093\", uri=\"/dir/index.html\", qop=auth, nc=00000001, \
                      cnonce=\"0a4f113b\", response=\"6629fae49393a05397450978507c4ef1\", \
                      opaque=\"5ccc069c403ebaf9f0171e9517f40e41\"";
        let nonce = "dcd98b7102dd2f0e8b11d0f600bfb0c093";
        assert!(digest_matches(header, "GET", nonce, "Circle Of Life"));
        assert!(!digest_matches(header, "GET", nonce, "Circle of life"));
        assert!(!digest_matches(header, "POST", nonce, "Circle Of Life"));
    }

    #[test]
    fn lets_everyone_in_when_off() {
        let gate = CodeGate::new(Arc::new(Hub::new("Test".into(), None, 0.5)), false);
        assert!(gate.admit("SETUP", None, "iPhone").is_ok());
    }

    #[test]
    fn the_right_code_works_once() {
        let gate = gate();
        let first = gate.admit("SETUP", None, "iPhone").unwrap_err();
        let code = current_code(&gate);
        assert_eq!(code.len(), 4);
        let accepted = answer(&first, &code);
        assert!(gate.admit("SETUP", Some(&accepted), "iPhone").is_ok());

        // Replaying the same answer starts over with a new code and fails.
        assert!(gate.admit("SETUP", Some(&accepted), "iPhone").is_err());
    }

    #[test]
    fn an_answer_to_an_old_challenge_is_no_good() {
        let gate = gate();
        let old = gate.admit("SETUP", None, "iPhone").unwrap_err();
        let code = current_code(&gate);
        gate.admit("SETUP", None, "iPhone").unwrap_err();
        assert!(gate.admit("SETUP", Some(&answer(&old, &code)), "iPhone").is_err());
    }

    #[test]
    fn another_device_shares_the_code_on_screen() {
        let gate = gate();
        let first = gate.admit("SETUP", None, "iPhone").unwrap_err();
        let code = current_code(&gate);
        let second = gate.admit("SETUP", None, "iPad").unwrap_err();
        assert_eq!(current_code(&gate), code);
        assert_eq!(gate.state.lock().unwrap().pending.as_ref().unwrap().device_name, "iPad");

        // Only the newest challenge can be answered.
        assert!(gate.admit("SETUP", Some(&answer(&first, &code)), "iPhone").is_err());
        let third = gate.admit("SETUP", None, "iPad").unwrap_err();
        assert_ne!(digest_nonce(&second), digest_nonce(&third));
        assert!(gate.admit("SETUP", Some(&answer(&third, &code)), "iPad").is_ok());
    }

    #[test]
    fn starting_over_earns_no_extra_guesses() {
        let gate = gate();
        let challenge = gate.admit("SETUP", None, "Attacker 1").unwrap_err();
        let code = current_code(&gate);
        let wrong = if code == "0000" { "0001" } else { "0000" };
        gate.admit("SETUP", Some(&answer(&challenge, wrong)), "Attacker 1").unwrap_err();

        // Coming back under another name keeps the same code and the same count.
        let challenge = gate.admit("SETUP", None, "Attacker 2").unwrap_err();
        assert_eq!(current_code(&gate), code);
        gate.admit("SETUP", Some(&answer(&challenge, wrong)), "Attacker 2").unwrap_err();

        // So does waiting for the code to expire and getting a new one.
        gate.state.lock().unwrap().pending.as_mut().unwrap().expires = Instant::now();
        let challenge = gate.admit("SETUP", None, "Attacker 3").unwrap_err();
        let code = current_code(&gate);
        let wrong = if code == "0000" { "0001" } else { "0000" };
        gate.admit("SETUP", Some(&answer(&challenge, wrong)), "Attacker 3").unwrap_err();
        let state = gate.state.lock().unwrap();
        assert!(state.locked_until.is_some());
        assert!(state.pending.is_none());
    }

    #[test]
    fn wrong_guesses_lead_to_a_growing_pause() {
        let gate = gate();
        let mut challenge = gate.admit("SETUP", None, "iPhone").unwrap_err();
        let right = current_code(&gate);
        let wrong = if right == "0000" { "0001" } else { "0000" };
        for _ in 0..MAX_ATTEMPTS {
            challenge = gate.admit("SETUP", Some(&answer(&challenge, wrong)), "iPhone").unwrap_err();
        }
        {
            let state = gate.state.lock().unwrap();
            assert!(state.pending.is_none());
            assert_eq!(state.lockout, FIRST_LOCKOUT);
            assert!(state.locked_until.is_some());
        }
        // Even the old right code can't get in during the pause.
        assert!(gate.admit("SETUP", Some(&answer(&challenge, &right)), "iPhone").is_err());

        // After the pause there's a new code, and another round pauses for longer.
        gate.state.lock().unwrap().locked_until = Some(Instant::now());
        let mut challenge = gate.admit("SETUP", None, "iPhone").unwrap_err();
        let right = current_code(&gate);
        let wrong = if right == "0000" { "0001" } else { "0000" };
        for _ in 0..MAX_ATTEMPTS {
            challenge = gate.admit("SETUP", Some(&answer(&challenge, wrong)), "iPhone").unwrap_err();
        }
        assert_eq!(gate.state.lock().unwrap().lockout, FIRST_LOCKOUT * 2);
    }

    #[test]
    fn cancelling_withdraws_the_code() {
        let gate = gate();
        let challenge = gate.admit("SETUP", None, "iPhone").unwrap_err();
        let code = current_code(&gate);
        gate.cancel();
        assert!(gate.admit("SETUP", Some(&answer(&challenge, &code)), "iPhone").is_err());
    }
}
