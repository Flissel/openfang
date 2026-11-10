//! Prueft jeden Katalog-Server einer Recherche-Datei auf dem Weg, den auch
//! das Einrichten nimmt, und schreibt `pruefung` in jede Zeile mit `server_url`.
//!
//! Aufruf: `cargo run -p openfang-extensions --example katalog_pruefen -- <recherche.json>`
//!
//! Es wird nie ein Client registriert; Antworttexte werden weder gespeichert
//! noch ausgegeben, nur Klassen.

use openfang_extensions::katalog_pruefung::{einordnen, Anmeldung, Antwort, Einordnung, Ergebnis};
use openfang_extensions::mcp_oauth::{self, OAuthError, UrlPolicy};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;

/// Obergrenze je Zeile (Ermittlung + eigenes initialize), zusaetzlich zu den
/// Einzel-Timeouts des Clients.
const ZEILEN_LIMIT: Duration = Duration::from_secs(90);

#[tokio::main]
async fn main() {
    let Some(pfad) = std::env::args().nth(1) else {
        eprintln!("Aufruf: katalog_pruefen <recherche.json>");
        std::process::exit(2);
    };
    let Ok(text) = std::fs::read_to_string(&pfad) else {
        eprintln!("Datei nicht lesbar");
        std::process::exit(1);
    };
    let Ok(Value::Array(mut zeilen)) = serde_json::from_str::<Value>(&text) else {
        eprintln!("Datei ist kein JSON-Array");
        std::process::exit(1);
    };

    let policy = UrlPolicy::strict();
    let Ok(http) = mcp_oauth::http_client(&policy) else {
        eprintln!("HTTP-Client nicht baubar");
        std::process::exit(1);
    };
    let datum = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut summe: BTreeMap<&'static str, usize> = BTreeMap::new();

    for zeile in zeilen.iter_mut() {
        let url = zeile
            .get("server_url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() {
            continue;
        }
        let name = zeile
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string();
        let anmeldung =
            Anmeldung::aus_text(zeile.get("anmeldung").and_then(Value::as_str).unwrap_or(""));

        let (einordnung, scopes) = if url.contains('{') {
            // Kein Netzaufruf fuer Adressen mit Platzhalter.
            (einordnen(&url, anmeldung, None, None), Vec::new())
        } else {
            match tokio::time::timeout(ZEILEN_LIMIT, messen(&http, &policy, &url, anmeldung)).await
            {
                Ok(r) => r,
                Err(_) => (
                    einordnen(&url, anmeldung, None, Some(Antwort::Netz)),
                    Vec::new(),
                ),
            }
        };

        println!(
            "{} {} {}",
            name,
            einordnung.ergebnis.als_text(),
            einordnung.klasse
        );
        *summe.entry(einordnung.ergebnis.als_text()).or_default() += 1;
        if let Some(obj) = zeile.as_object_mut() {
            obj.insert(
                "pruefung".into(),
                json!({
                    "ergebnis": einordnung.ergebnis.als_text(),
                    "klasse": einordnung.klasse,
                    "scopes_supported": scopes,
                    "datum": datum,
                }),
            );
        }
    }

    println!("--- Zusammenfassung ---");
    for e in [
        Ergebnis::EinrichtbarOauth,
        Ergebnis::EinrichtbarSchluessel,
        Ergebnis::EinrichtbarOhneAnmeldung,
        Ergebnis::BrauchtRegistrierung,
        Ergebnis::NichtErreichbar,
        Ergebnis::Unklar,
        Ergebnis::AdresseJeKunde,
    ] {
        println!("{} {}", e.als_text(), summe.get(e.als_text()).unwrap_or(&0));
    }

    let Ok(ausgabe) = serde_json::to_string_pretty(&zeilen) else {
        eprintln!("Serialisierung fehlgeschlagen");
        std::process::exit(1);
    };
    if std::fs::write(&pfad, ausgabe + "\n").is_err() {
        eprintln!("Datei nicht schreibbar");
        std::process::exit(1);
    }
}

/// Ermittlung wie beim Einrichten; nur bei Fehlschlag ein eigenes `initialize`.
async fn messen(
    http: &mcp_oauth::OAuthHttp,
    policy: &UrlPolicy,
    url: &str,
    anmeldung: Anmeldung,
) -> (Einordnung, Vec<String>) {
    match mcp_oauth::discover(http, policy, url).await {
        Ok(rec) => (
            einordnen(
                url,
                anmeldung,
                Some(Ok(rec.registration_endpoint.is_some())),
                None,
            ),
            rec.scopes_supported,
        ),
        Err(err) => {
            let antwort = match mcp_oauth::probe_initialize_status(http, policy, url).await {
                Ok(s) => Antwort::Status(s),
                Err(OAuthError::Discovery("weiterleitung")) => Antwort::Umleitung,
                Err(_) => Antwort::Netz,
            };
            (
                einordnen(url, anmeldung, Some(Err(err.class())), Some(antwort)),
                Vec::new(),
            )
        }
    }
}
