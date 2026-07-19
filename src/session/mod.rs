//! In-memory cookie sessions (single-threaded event hub).

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const COOKIE_NAME: &str = "session_id";
const IDLE_LIMIT: Duration = Duration::from_secs(30 * 60);

static TICK: AtomicU64 = AtomicU64::new(1);

/// Key/value bag attached to one browser session.
#[derive(Default, Clone)]
pub struct Bag {
    values: HashMap<String, String>,
}

impl Bag {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    pub fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.values.insert(key.into(), value.into());
    }
}

struct Slot {
    touched: Instant,
    bag: Bag,
}

/// Process-wide session table. Safe to share via `Rc<RefCell<_>>` on one thread.
#[derive(Default)]
pub struct Vault {
    slots: HashMap<String, Slot>,
}

impl Vault {
    pub fn new() -> Self {
        Self {
            slots: HashMap::new(),
        }
    }

    /// Resume a cookie session or mint a fresh id. Touches activity time.
    /// Expiry is swept by the caller on its own cadence (see `sweep`), not
    /// here — an O(n) scan on every single request degrades badly once the
    /// table has many entries (e.g. under sustained load from clients that
    /// don't return a cookie, such as `siege -b`).
    pub fn resume_or_mint(&mut self, cookie_header: Option<&str>) -> String {
        if let Some(id) = cookie_header.and_then(read_session_id) {
            if let Some(slot) = self.slots.get_mut(id) {
                if slot.touched.elapsed() < IDLE_LIMIT {
                    slot.touched = Instant::now();
                    return id.to_string();
                }
            }
            self.slots.remove(id);
        }
        let id = mint_token();
        self.slots.insert(
            id.clone(),
            Slot {
                touched: Instant::now(),
                bag: Bag::default(),
            },
        );
        id
    }

    pub fn bag_mut(&mut self, id: &str) -> Option<&mut Bag> {
        self.slots.get_mut(id).map(|s| {
            s.touched = Instant::now();
            &mut s.bag
        })
    }

    pub fn sweep(&mut self) {
        self.slots
            .retain(|_, slot| slot.touched.elapsed() < IDLE_LIMIT);
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }
}

/// `Set-Cookie` value for the given session id.
pub fn set_cookie_header(id: &str) -> String {
    format!("{COOKIE_NAME}={id}; Path=/; HttpOnly; SameSite=Lax")
}

fn read_session_id(header: &str) -> Option<&str> {
    for piece in header.split(';') {
        let piece = piece.trim();
        let Some((name, value)) = piece.split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(COOKIE_NAME) {
            let value = value.trim();
            if !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Some(value);
            }
        }
    }
    None
}

fn mint_token() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let n = TICK.fetch_add(1, Ordering::Relaxed);
    let pid = unsafe { libc::getpid() as u64 };
    // Mix clocks / pid / counter — not cryptographic, fine for demo sessions.
    let a = nanos.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(pid);
    let b = n.wrapping_mul(0xC2B2_AE3D_27D4_EB4F).wrapping_add(nanos);
    format!("{a:016x}{b:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_session_cookie_among_others() {
        assert_eq!(
            read_session_id("theme=dark; session_id=deadbeefcafebabe; x=1"),
            Some("deadbeefcafebabe")
        );
        assert!(read_session_id("session_id=not!hex").is_none());
    }

    #[test]
    fn mints_when_missing_and_reuses_when_present() {
        let mut vault = Vault::new();
        let a = vault.resume_or_mint(None);
        assert_eq!(a.len(), 32);
        let cookie = format!("session_id={a}");
        let b = vault.resume_or_mint(Some(&cookie));
        assert_eq!(a, b);
        assert_eq!(vault.len(), 1);
    }

    #[test]
    fn bag_survives_across_touches() {
        let mut vault = Vault::new();
        let id = vault.resume_or_mint(None);
        vault.bag_mut(&id).unwrap().set("hits", "1");
        let id2 = vault.resume_or_mint(Some(&format!("session_id={id}")));
        assert_eq!(id, id2);
        assert_eq!(vault.bag_mut(&id2).unwrap().get("hits"), Some("1"));
    }

    #[test]
    fn set_cookie_format() {
        let h = set_cookie_header("abc");
        assert!(h.starts_with("session_id=abc;"));
        assert!(h.contains("HttpOnly"));
        assert!(h.contains("Path=/"));
    }
}
