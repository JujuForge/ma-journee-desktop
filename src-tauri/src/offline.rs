//! Page hors ligne de la fenetre principale.
//!
//! Au tout premier lancement (Service Worker de l'app web pas encore installe), un reseau
//! injoignable laisserait la page d'erreur brute du moteur web. L'app web signale son chargement
//! par la commande `frontend_ready` ; sans ce signal (voir [on_page_load]), la fenetre affiche
//! cette page locale, servie par le protocole `mjoffline` (embarquee, fonctionne hors ligne).

use std::{
    borrow::Cow,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::Duration,
};

use tauri::{http, webview::PageLoadEvent};

pub const PROTOCOL: &str = "mjoffline";
/// Delai maximal laisse a l'app web pour se charger (connexion bloquee sans reponse).
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(15);
/// Delai apres la fin du chargement (reussi ou non) pour recevoir `frontend_ready`.
const FINISHED_GRACE: Duration = Duration::from_secs(2);
const PAGE: &str = include_str!("../assets/offline.html");
const LOGO: &[u8] = include_bytes!("../icons/128x128@2x.png");

static FRONTEND_READY: AtomicBool = AtomicBool::new(false);
/// Incremente a chaque chargement de l'app web : un minuteur d'un chargement precedent
/// (rechargement, bouton Reessayer) ne doit pas agir sur le suivant.
static LOAD_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Racine du protocole selon la convention de wry (`http://<schema>.localhost` sous Windows).
pub fn page_base() -> String {
    if cfg!(windows) { format!("http://{PROTOCOL}.localhost/") } else { format!("{PROTOCOL}://localhost/") }
}

pub fn is_offline_page(url: &tauri::Url) -> bool {
    url.as_str().starts_with(&page_base())
}

pub fn serve_asset(request: &http::Request<Vec<u8>>, frontend_url: &str) -> http::Response<Cow<'static, [u8]>> {
    let response = match request.uri().path() {
        "/" => http::Response::builder()
            .header(http::header::CONTENT_TYPE, "text/html; charset=utf-8")
            .body(Cow::Owned(PAGE.replace("{{FRONTEND_URL}}", frontend_url).into_bytes())),
        "/logo.png" => {
            http::Response::builder().header(http::header::CONTENT_TYPE, "image/png").body(Cow::Borrowed(LOGO))
        }
        _ => http::Response::builder().status(http::StatusCode::NOT_FOUND).body(Cow::Borrowed(&[][..])),
    };
    response.expect("en-tetes statiques valides")
}

/// Page d'entree de l'app web (celle qui charge main.js et appelle `frontend_ready`). Les autres
/// pages du site (404...) ne l'appellent pas et ne doivent pas declencher la page hors ligne.
fn is_app_entry(url: &tauri::Url, frontend_url: &str) -> bool {
    let Ok(frontend) = tauri::Url::parse(frontend_url) else { return false };
    url.scheme() == frontend.scheme()
        && url.host_str() == frontend.host_str()
        && url.port_or_known_default() == frontend.port_or_known_default()
        && matches!(url.path(), "/" | "/index.html")
}

/// A brancher sur `on_page_load` de la fenetre principale.
///
/// Le moteur web signale `Finished` meme quand le chargement echoue (connexion refusee, DNS) :
/// l'absence de `frontend_ready` peu apres suffit alors, sans attendre [OFFLINE_TIMEOUT], qui ne
/// sert plus qu'aux connexions bloquees sans reponse. La marge [FINISHED_GRACE] couvre l'appel IPC
/// de main.js, envoye avant la fin du chargement mais traite de facon asynchrone.
pub fn on_page_load(window: tauri::WebviewWindow, url: &tauri::Url, event: PageLoadEvent, frontend_url: &str) {
    if !is_app_entry(url, frontend_url) {
        return;
    }
    let (generation, delay) = match event {
        PageLoadEvent::Started => {
            FRONTEND_READY.store(false, Ordering::SeqCst);
            (LOAD_GENERATION.fetch_add(1, Ordering::SeqCst) + 1, OFFLINE_TIMEOUT)
        }
        PageLoadEvent::Finished => (LOAD_GENERATION.load(Ordering::SeqCst), FINISHED_GRACE),
    };
    thread::spawn(move || {
        thread::sleep(delay);
        if FRONTEND_READY.load(Ordering::SeqCst) || LOAD_GENERATION.load(Ordering::SeqCst) != generation {
            return;
        }
        // Une seule bascule par chargement : l'autre minuteur trouvera une generation perimee.
        if LOAD_GENERATION.compare_exchange(generation, generation + 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            return;
        }
        log::warn!("App web non chargee, page hors ligne affichee");
        let url: tauri::Url = page_base().parse().expect("URL locale valide");
        if let Err(e) = window.navigate(url) {
            log::error!("Echec affichage de la page hors ligne : {}", e);
        }
    });
}

/// Appelee par l'app web une fois chargee (js/main.js).
#[tauri::command]
pub fn frontend_ready() {
    FRONTEND_READY.store(true, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_is_served_with_frontend_url_and_unknown_paths_are_not_found() {
        let get =
            |path: &str| serve_asset(&http::Request::get(path).body(Vec::new()).unwrap(), "https://majournee.com");
        let page = get("/");
        assert_eq!(page.status(), http::StatusCode::OK);
        let html = String::from_utf8(page.body().to_vec()).unwrap();
        assert!(html.contains("'https://majournee.com'"));
        assert!(!html.contains("{{FRONTEND_URL}}"));
        assert_eq!(get("/logo.png").status(), http::StatusCode::OK);
        assert_eq!(get("/inconnu").status(), http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_the_app_entry_page_arms_the_timeout() {
        let entry = |url: &str| is_app_entry(&url.parse().unwrap(), "https://majournee.com");
        assert!(entry("https://majournee.com/"));
        assert!(entry("https://majournee.com/index.html#taches"));
        assert!(!entry("https://majournee.com/404.html"));
        assert!(!entry("https://mcp.majournee.com/"));
        assert!(!entry("http://majournee.com/"));
        assert!(!entry(&page_base()));
    }

    #[test]
    fn offline_page_is_recognized() {
        assert!(is_offline_page(&page_base().parse().unwrap()));
        assert!(!is_offline_page(&"https://majournee.com/".parse().unwrap()));
    }
}
