//! Katalog-Pruefung: reine Einordnung des Ergebnisses einer Server-Pruefung.
//!
//! Dieses Modul macht keine Netzaufrufe. Das Beispielprogramm
//! `katalog_pruefen` liefert die Messwerte (Ermittlung, unauthentifiziertes
//! `initialize`), `einordnen` macht daraus Ergebnis und Klasse.

use serde::{Deserialize, Serialize};

/// Anmeldeart laut Recherche.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Anmeldung {
    Oauth,
    Schluessel,
    Unbekannt,
}

impl Anmeldung {
    /// Liest den Recherche-Wert (`oauth` | `schluessel` | sonst unbekannt).
    pub fn aus_text(s: &str) -> Self {
        match s {
            "oauth" => Anmeldung::Oauth,
            "schluessel" => Anmeldung::Schluessel,
            _ => Anmeldung::Unbekannt,
        }
    }
}

/// Antwort des unauthentifizierten `initialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Antwort {
    Status(u16),
    Umleitung,
    Netz,
}

/// Ergebnis einer Pruefung (serialisiert snake_case).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ergebnis {
    EinrichtbarOauth,
    EinrichtbarSchluessel,
    EinrichtbarOhneAnmeldung,
    BrauchtRegistrierung,
    NichtErreichbar,
    Unklar,
    AdresseJeKunde,
}

impl Ergebnis {
    pub fn als_text(self) -> &'static str {
        match self {
            Ergebnis::EinrichtbarOauth => "einrichtbar_oauth",
            Ergebnis::EinrichtbarSchluessel => "einrichtbar_schluessel",
            Ergebnis::EinrichtbarOhneAnmeldung => "einrichtbar_ohne_anmeldung",
            Ergebnis::BrauchtRegistrierung => "braucht_registrierung",
            Ergebnis::NichtErreichbar => "nicht_erreichbar",
            Ergebnis::Unklar => "unklar",
            Ergebnis::AdresseJeKunde => "adresse_je_kunde",
        }
    }
}

/// Ergebnis plus kurze Klasse (nie Antworttexte).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Einordnung {
    pub ergebnis: Ergebnis,
    pub klasse: String,
}

fn e(ergebnis: Ergebnis, klasse: impl Into<String>) -> Einordnung {
    Einordnung {
        ergebnis,
        klasse: klasse.into(),
    }
}

/// Ordnet die Messwerte ein.
///
/// - `entdeckt`: `None` = nicht versucht; `Some(Ok(dcr))` = Ermittlung gelang,
///   `dcr` = Registrierungsendpunkt vorhanden; `Some(Err(klasse))` = Ermittlung
///   scheiterte, Text ist `OAuthError::class()`.
/// - `initialize`: Antwort des eigenen unauthentifizierten `initialize`
///   (nur noetig, wenn die Ermittlung nicht gelang).
pub fn einordnen(
    url: &str,
    anmeldung: Anmeldung,
    entdeckt: Option<Result<bool, String>>,
    initialize: Option<Antwort>,
) -> Einordnung {
    if url.contains('{') {
        return e(Ergebnis::AdresseJeKunde, "adresse_je_kunde");
    }
    if let Some(Ok(dcr)) = &entdeckt {
        return if *dcr {
            e(Ergebnis::EinrichtbarOauth, "oauth_dcr")
        } else {
            e(Ergebnis::BrauchtRegistrierung, "oauth_ohne_dcr")
        };
    }
    match initialize {
        None => e(Ergebnis::Unklar, "nicht_geprueft"),
        Some(Antwort::Umleitung) => e(Ergebnis::NichtErreichbar, "umleitung"),
        Some(Antwort::Netz) => e(Ergebnis::NichtErreichbar, "netz"),
        // Wird eine Anmeldung erwartet, sagt ein 2xx auf `initialize` nichts
        // ueber die Werkzeuge: unklar statt "ohne Anmeldung".
        Some(Antwort::Status(s)) if (200..300).contains(&s) => match anmeldung {
            Anmeldung::Unbekannt => e(Ergebnis::EinrichtbarOhneAnmeldung, "offen"),
            Anmeldung::Oauth | Anmeldung::Schluessel => {
                e(Ergebnis::Unklar, "antwortet_ohne_anmeldung")
            }
        },
        Some(Antwort::Status(s)) if s == 401 || s == 403 => match anmeldung {
            Anmeldung::Schluessel => e(Ergebnis::EinrichtbarSchluessel, "schluessel"),
            Anmeldung::Unbekannt => e(Ergebnis::Unklar, "anmeldeart_unbekannt"),
            Anmeldung::Oauth => match entdeckt {
                Some(Err(klasse)) => e(Ergebnis::Unklar, klasse),
                _ => e(Ergebnis::Unklar, "oauth_nicht_ermittelt"),
            },
        },
        Some(Antwort::Status(s)) if (400..500).contains(&s) => {
            e(Ergebnis::NichtErreichbar, format!("status_{s}"))
        }
        Some(Antwort::Status(s)) => e(Ergebnis::Unklar, format!("status_{s}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const U: &str = "https://mcp.example.test/mcp";
    const WEITER: &str = "ermittlung fehlgeschlagen: weiterleitung";
    const FEHL: &str = "ermittlung fehlgeschlagen: kein s256";

    fn ist(x: Einordnung, erg: Ergebnis, klasse: &str) {
        assert_eq!((x.ergebnis, x.klasse.as_str()), (erg, klasse));
    }

    #[test]
    fn adresse_je_kunde_ohne_netz() {
        let x = einordnen(
            "https://{tenant}.example.test/mcp",
            Anmeldung::Oauth,
            None,
            None,
        );
        ist(x, Ergebnis::AdresseJeKunde, "adresse_je_kunde");
    }

    #[test]
    fn ermittlung_entscheidet_oauth() {
        let a = einordnen(U, Anmeldung::Oauth, Some(Ok(true)), None);
        assert_eq!(a.ergebnis, Ergebnis::EinrichtbarOauth);
        let b = einordnen(U, Anmeldung::Oauth, Some(Ok(false)), None);
        assert_eq!(b.ergebnis, Ergebnis::BrauchtRegistrierung);
    }

    #[test]
    fn umleitung_und_netz() {
        let x = einordnen(
            U,
            Anmeldung::Oauth,
            Some(Err(WEITER.into())),
            Some(Antwort::Umleitung),
        );
        ist(x, Ergebnis::NichtErreichbar, "umleitung");
        let y = einordnen(
            U,
            Anmeldung::Oauth,
            Some(Err("nicht erreichbar: verbindung".into())),
            Some(Antwort::Netz),
        );
        ist(y, Ergebnis::NichtErreichbar, "netz");
    }

    #[test]
    fn status_tabelle() {
        let f = |a, s| einordnen(U, a, Some(Err(FEHL.into())), Some(Antwort::Status(s)));
        // R4: 2xx trotz erwarteter Anmeldung ist unklar (Werkzeuge brauchen sie evtl. doch)
        ist(f(Anmeldung::Oauth, 200), Ergebnis::Unklar, "antwortet_ohne_anmeldung");
        ist(
            f(Anmeldung::Schluessel, 200),
            Ergebnis::Unklar,
            "antwortet_ohne_anmeldung",
        );
        ist(
            f(Anmeldung::Unbekannt, 200),
            Ergebnis::EinrichtbarOhneAnmeldung,
            "offen",
        );
        for s in [401, 403] {
            ist(
                f(Anmeldung::Schluessel, s),
                Ergebnis::EinrichtbarSchluessel,
                "schluessel",
            );
            ist(
                f(Anmeldung::Unbekannt, s),
                Ergebnis::Unklar,
                "anmeldeart_unbekannt",
            );
            // R2: OAuth angegeben, Ermittlung gescheitert -> unklar mit Ermittlungsklasse
            ist(f(Anmeldung::Oauth, s), Ergebnis::Unklar, FEHL);
        }
        ist(f(Anmeldung::Oauth, 404), Ergebnis::NichtErreichbar, "status_404");
        ist(f(Anmeldung::Oauth, 500), Ergebnis::Unklar, "status_500");
    }

    #[test]
    fn ergebnis_serialisiert_snake_case() {
        let s = serde_json::to_string(&Ergebnis::EinrichtbarOhneAnmeldung).unwrap();
        assert_eq!(s, "\"einrichtbar_ohne_anmeldung\"");
        assert_eq!(Ergebnis::AdresseJeKunde.als_text(), "adresse_je_kunde");
    }
}
