//! Bausteine fuer die OAuth-Anmeldung von Integrationen: `state`-Speicher,
//! Einmal-Links und Erneuerungssperren.
//!
//! Sicherheit: Kein Wert (state, verifier, Token) erscheint in einem `Debug`.
//! `PendingLogin` und `OneTimeLink` leiten deshalb bewusst kein `Debug` ab.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Gueltigkeit eines `state` bzw. Einmal-Links.
pub const LOGIN_TTL: Duration = Duration::from_secs(600);

/// Obergrenze fuer offene Anmeldungen bzw. Einmal-Links.
const MAX_ENTRIES: usize = 256;

/// Verwirft Abgelaufene, dann die aeltesten, bis Platz fuer einen Eintrag ist.
fn prune<V>(map: &mut HashMap<String, V>, created: impl Fn(&V) -> Instant) {
    map.retain(|_, v| created(v).elapsed() <= LOGIN_TTL);
    while map.len() >= MAX_ENTRIES {
        let Some(oldest) = map.iter().min_by_key(|(_, v)| created(v)).map(|(k, _)| k.clone()) else { break };
        map.remove(&oldest);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Eine laufende Anmeldung (kein `Debug`: enthaelt den PKCE-Verifier).
pub struct PendingLogin {
    pub integration: String,
    pub verifier: zeroize::Zeroizing<String>,
    pub redirect_uri: String,
    pub created: Instant,
}

/// `state` -> laufende Anmeldung.
#[derive(Default)]
pub struct PendingLogins {
    map: Mutex<HashMap<String, PendingLogin>>,
}

impl PendingLogins {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, state: &str, login: PendingLogin) {
        let mut map = lock(&self.map);
        prune(&mut map, |l| l.created);
        map.insert(state.to_string(), login);
    }

    /// Atomar und einmalig: entfernt den Eintrag IMMER (fail closed), liefert
    /// ihn nur bei passender Integration und nicht abgelaufener TTL.
    pub fn take(&self, state: &str, integration: &str, ttl: Duration) -> Option<PendingLogin> {
        let login = lock(&self.map).remove(state)?;
        (login.integration == integration && login.created.elapsed() <= ttl).then_some(login)
    }
}

/// Einmal-Link (kein `Debug`).
pub struct OneTimeLink {
    pub integration: String,
    pub reference: String,
    pub created: Instant,
}

/// Token -> Einmal-Link.
#[derive(Default)]
pub struct OneTimeLinks {
    map: Mutex<HashMap<String, OneTimeLink>>,
}

impl OneTimeLinks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&self, token: &str, link: OneTimeLink) {
        let mut map = lock(&self.map);
        prune(&mut map, |l| l.created);
        map.insert(token.to_string(), link);
    }

    /// GET: liefert die Referenz eines gueltigen Links, verbraucht ihn nicht.
    pub fn peek(&self, token: &str, integration: &str, ttl: Duration) -> Option<String> {
        let map = lock(&self.map);
        let l = map.get(token)?;
        (l.integration == integration && l.created.elapsed() <= ttl).then(|| l.reference.clone())
    }

    /// POST: verbraucht den Link immer, liefert die Referenz nur wenn gueltig.
    pub fn take(&self, token: &str, integration: &str, ttl: Duration) -> Option<String> {
        let l = lock(&self.map).remove(token)?;
        (l.integration == integration && l.created.elapsed() <= ttl).then_some(l.reference)
    }
}

/// Eine Erneuerungssperre je Integration.
#[derive(Default)]
pub struct RefreshLocks {
    map: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl RefreshLocks {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn for_integration(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.map)
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// Callback-URL des Daemons. Immer Loopback (`127.0.0.1`); jeder andere Host
/// fuehrt zu einem Fehler.
pub fn callback_url(api_listen: &str, id: &str) -> Result<String, &'static str> {
    let (host, port) = api_listen.rsplit_once(':').ok_or("api_listen ohne port")?;
    if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u16>().is_err() {
        return Err("api_listen ohne port");
    }
    let host = host.trim_start_matches('[').trim_end_matches(']');
    match host {
        "" | "127.0.0.1" | "localhost" | "0.0.0.0" | "::1" | "::" => {
            Ok(format!("http://127.0.0.1:{port}/api/integrations/{id}/oauth/callback"))
        }
        _ => Err("api_listen nicht loopback"),
    }
}

/// Statischer Schluessel: nicht leer, keine Steuerzeichen, <= 4096 Bytes.
pub fn validate_static_key(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        return Err("Wert darf nicht leer sein");
    }
    if value.len() > 4096 {
        return Err("Wert ist zu lang");
    }
    if value.chars().any(|c| c.is_control()) {
        return Err("Wert enthaelt Steuerzeichen");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn login(i: &str) -> PendingLogin {
        PendingLogin { integration: i.into(), verifier: zeroize::Zeroizing::new("V".into()), redirect_uri: "r".into(), created: Instant::now() }
    }

    #[test]
    fn state_is_single_use_and_bound_to_integration() {
        let p = PendingLogins::new();
        p.insert("S1", login("vercel"));
        assert!(p.take("S1", "github", LOGIN_TTL).is_none(), "falsche Integration");
        assert!(p.take("S1", "vercel", LOGIN_TTL).is_none(), "nach Fehlversuch verbraucht (fail closed)");
        p.insert("S2", login("vercel"));
        assert!(p.take("S2", "vercel", LOGIN_TTL).is_some());
        assert!(p.take("S2", "vercel", LOGIN_TTL).is_none(), "einmalig");
    }

    #[test]
    fn state_expires() {
        let p = PendingLogins::new();
        let mut l = login("vercel");
        // Frisch gebootete Maschine: Instant kann zu klein sein -> Assertion ueberspringen.
        let Some(old) = Instant::now().checked_sub(Duration::from_secs(601)) else { return };
        l.created = old;
        p.insert("S", l);
        assert!(p.take("S", "vercel", LOGIN_TTL).is_none());
    }

    #[test]
    fn one_time_link_peek_does_not_consume_take_does() {
        let l = OneTimeLinks::new();
        l.insert("T", OneTimeLink { integration: "github".into(), reference: "INTEGRATION_GITHUB_PAT".into(), created: Instant::now() });
        assert_eq!(l.peek("T", "github", LOGIN_TTL).as_deref(), Some("INTEGRATION_GITHUB_PAT"));
        assert_eq!(l.take("T", "github", LOGIN_TTL).as_deref(), Some("INTEGRATION_GITHUB_PAT"));
        assert!(l.take("T", "github", LOGIN_TTL).is_none());
        assert!(l.peek("T", "github", LOGIN_TTL).is_none());
    }

    #[tokio::test]
    async fn refresh_lock_is_shared_per_integration() {
        let r = RefreshLocks::new();
        let a = r.for_integration("vercel");
        let b = r.for_integration("vercel");
        let c = r.for_integration("linear");
        assert!(Arc::ptr_eq(&a, &b));
        assert!(!Arc::ptr_eq(&a, &c));
        let _g = a.lock().await;
        assert!(b.try_lock().is_err());
    }

    #[test]
    fn callback_url_is_always_loopback_or_fails() {
        let want = "http://127.0.0.1:4200/api/integrations/vercel/oauth/callback";
        for l in ["127.0.0.1:4200", "0.0.0.0:4200", "[::]:4200", "[::1]:4200", "localhost:4200", ":4200"] {
            assert_eq!(callback_url(l, "vercel").as_deref(), Ok(want), "{l}");
        }
        assert_eq!(callback_url("192.168.1.5:4200", "vercel"), Err("api_listen nicht loopback"));
        assert_eq!(callback_url("example.com:4200", "vercel"), Err("api_listen nicht loopback"));
        assert_eq!(callback_url("127.0.0.1", "vercel"), Err("api_listen ohne port"));
        assert_eq!(callback_url("127.0.0.1:abc", "vercel"), Err("api_listen ohne port"));
    }

    #[test]
    fn maps_are_bounded() {
        let p = PendingLogins::new();
        let l = OneTimeLinks::new();
        for i in 0..300 {
            p.insert(&format!("S{i}"), login("vercel"));
            l.insert(&format!("T{i}"), OneTimeLink { integration: "github".into(), reference: "R".into(), created: Instant::now() });
        }
        assert!(p.map.lock().unwrap().len() <= 256);
        assert!(l.map.lock().unwrap().len() <= 256);
        assert!(p.take("S299", "vercel", LOGIN_TTL).is_some(), "neuester bleibt");
        assert!(l.take("T299", "github", LOGIN_TTL).is_some(), "neuester bleibt");
    }

    #[test]
    fn static_key_validation() {
        assert!(validate_static_key("ghp_abc").is_ok());
        assert!(validate_static_key("").is_err());
        assert!(validate_static_key("a\r\nb").is_err());
        assert!(validate_static_key(&"x".repeat(4097)).is_err());
    }
}
