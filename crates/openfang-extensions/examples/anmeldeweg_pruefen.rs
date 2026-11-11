//! Prueft den Anmeldeweg ungepruefter OAuth-Vorlagen so, wie `oauth/start`
//! im Kernel ihn geht: Ermittlung, Selbstregistrierung (mit der echten
//! Rueckruf-Adresse des Daemons), Anmelde-Link. Danach wird die Anmeldeseite
//! einmal ohne Login aufgerufen (keine Weiterleitung folgen).
//!
//! Aufruf: `cargo run -p openfang-extensions --example anmeldeweg_pruefen -- <integrations-ordner> <ausgabe.json> [id ...]`
//!
//! Seiteneffekt: je Anbieter wird ein oeffentlicher Client registriert (wie
//! bei einem echten Klick auf "Anmelden"). Es werden keine Tokens geholt.
//! Ausgegeben werden nur Klassen: Status, OAuth-Fehlercodes, Feldnamen.

use openfang_extensions::mcp_oauth::{self, UrlPolicy};
use serde_json::{json, Value};
use std::time::Duration;

const API_LISTEN: &str = "127.0.0.1:4200";
const ZEILEN_LIMIT: Duration = Duration::from_secs(90);

struct Vorlage {
    id: String,
    url: String,
    scopes: Vec<String>,
}

fn vorlagen(ordner: &str, nur: &[String]) -> Vec<Vorlage> {
    let mut aus = Vec::new();
    let Ok(rd) = std::fs::read_dir(ordner) else {
        return aus;
    };
    let mut pfade: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    pfade.sort();
    for p in pfade {
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let Ok(tab) = toml::from_str::<toml::Table>(&text) else {
            eprintln!("nicht lesbar: {}", p.display());
            continue;
        };
        let v = toml::Value::Table(tab);
        let verified = v
            .get("catalog")
            .and_then(|c| c.get("verified"))
            .and_then(toml::Value::as_bool)
            .unwrap_or(true);
        let oauth = v
            .get("auth")
            .and_then(|a| a.get("type"))
            .and_then(toml::Value::as_str)
            == Some("oauth");
        let id = v
            .get("id")
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();
        // Mit expliziten ids auch gepruefte Vorlagen (zum Eichen an Vercel).
        if !oauth || (nur.is_empty() && verified) || (!nur.is_empty() && !nur.contains(&id)) {
            continue;
        }
        let url = v
            .get("transport")
            .and_then(|t| t.get("url"))
            .and_then(toml::Value::as_str)
            .unwrap_or("")
            .to_string();
        let scopes = v
            .get("auth")
            .and_then(|a| a.get("scopes"))
            .and_then(toml::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        aus.push(Vorlage { id, url, scopes });
    }
    aus
}

/// Nur OAuth-Fehlercode, Feldnamen und Stichworte aus einer Fehlerantwort.
fn fehler_klassen(body: &str) -> Value {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = v
        .get("error")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 40 && s.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
    let felder: Vec<String> = v
        .get("errors")
        .and_then(Value::as_object)
        .map(|o| o.keys().filter(|k| k.len() <= 40).cloned().collect())
        .unwrap_or_default();
    let klein = body.to_ascii_lowercase();
    let stichworte: Vec<&str> = [
        "https",
        "redirect_uri",
        "localhost",
        "127.0.0.1",
        "loopback",
        "client_name",
        "scope",
        "not allowed",
        "disabled",
        "whitelist",
        "allowlist",
    ]
    .into_iter()
    .filter(|w| klein.contains(w))
    .collect();
    json!({ "code": code, "felder": felder, "stichworte": stichworte })
}

async fn pruefen(
    http: &mcp_oauth::OAuthHttp,
    roh: &reqwest::Client,
    policy: &UrlPolicy,
    v: &Vorlage,
) -> Value {
    let redirect = format!(
        "http://{API_LISTEN}/api/integrations/{}/oauth/callback",
        v.id
    );
    let mut rec = match mcp_oauth::discover(http, policy, &v.url).await {
        Ok(r) => r,
        Err(e) => return json!({ "stufe": "ermittlung", "klasse": e.class() }),
    };
    rec.scopes = v.scopes.clone();
    if rec.registration_endpoint.is_none() {
        return json!({ "stufe": "registrierung", "klasse": "keine selbstregistrierung" });
    }
    if let Err(e) = mcp_oauth::register_client(http, policy, &mut rec, &redirect).await {
        // Gleiche Anfrage noch einmal roh, nur um die Fehlerklassen zu lesen.
        let mut details = Value::Null;
        if let Some(ep) = rec.registration_endpoint.as_deref() {
            if let Ok(r) = roh
                .post(ep)
                .header("accept", "application/json")
                .json(&mcp_oauth::registrierungs_body(&redirect, &rec.scopes))
                .send()
                .await
            {
                let text = r.text().await.unwrap_or_default();
                details = fehler_klassen(&text);
            }
        }
        return json!({ "stufe": "registrierung", "klasse": e.class(), "details": details });
    }
    let pkce = mcp_oauth::pkce_pair();
    let state = mcp_oauth::random_token();
    let link = match mcp_oauth::authorize_url(policy, &rec, &redirect, &state, &pkce.challenge) {
        Ok(u) => u,
        Err(e) => return json!({ "stufe": "anmelde_link", "klasse": e.class() }),
    };
    let r = match roh.get(&link).header("accept", "text/html").send().await {
        Ok(r) => r,
        Err(_) => return json!({ "stufe": "anmeldeseite", "klasse": "netz" }),
    };
    let status = r.status().as_u16();
    let ziel = r
        .headers()
        .get("location")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("")
        .to_string();
    if (300..400).contains(&status) {
        let zu_uns = ziel.starts_with(&redirect);
        let fehler = url::Url::parse(&ziel)
            .ok()
            .and_then(|u| {
                u.query_pairs()
                    .find(|(k, _)| k == "error")
                    .map(|(_, v)| v.to_string())
            })
            .filter(|s| s.len() <= 40 && s.chars().all(|c| c.is_ascii_lowercase() || c == '_'));
        let host = url::Url::parse(&ziel)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string));
        let klasse = match (zu_uns, &fehler) {
            (true, Some(_)) => "abgelehnt_per_rueckruf",
            (_, Some(_)) => "weiterleitung_mit_fehler",
            _ => "weiterleitung_zur_anmeldung",
        };
        return json!({ "stufe": "anmeldeseite", "klasse": klasse, "status": status, "ziel_host": host, "fehler": fehler, "link": link });
    }
    if (200..300).contains(&status) {
        // Ob Login-Formular oder Fehlerseite, entscheidet erst ein Browser
        // (Seiten spiegeln den Link samt redirect_uri in ihren Text).
        // Der Link enthaelt keine Geheimnisse: kein offener Vorgang im Daemon.
        return json!({ "stufe": "anmeldeseite", "klasse": "seite_200", "status": status, "link": link });
    }
    let text = r.text().await.unwrap_or_default();
    json!({ "stufe": "anmeldeseite", "klasse": "abgelehnt", "status": status, "details": fehler_klassen(&text) })
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Aufruf: anmeldeweg_pruefen <integrations-ordner> <ausgabe.json> [id ...]");
        std::process::exit(2);
    }
    let liste = vorlagen(&args[1], &args[3..]);
    let policy = UrlPolicy::strict();
    let http = mcp_oauth::http_client(&policy).expect("HTTP-Client");
    let roh = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        // Die Anmeldeseite oeffnet sonst der Browser des Nutzers.
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0 Safari/537.36")
        .build()
        .expect("roher Client");
    let mut aus = serde_json::Map::new();
    for v in &liste {
        let r = match tokio::time::timeout(ZEILEN_LIMIT, pruefen(&http, &roh, &policy, v)).await {
            Ok(r) => r,
            Err(_) => json!({ "stufe": "gesamt", "klasse": "zeitlimit" }),
        };
        println!(
            "{} {} {}",
            v.id,
            r["stufe"].as_str().unwrap_or("?"),
            r["klasse"].as_str().unwrap_or("?")
        );
        aus.insert(v.id.clone(), r);
    }
    std::fs::write(
        &args[2],
        serde_json::to_string_pretty(&Value::Object(aus)).unwrap() + "\n",
    )
    .expect("Ausgabe schreibbar");
}
