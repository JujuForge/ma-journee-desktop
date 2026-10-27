//! Verrouillage local de l'app (ecran de confidentialite sur un ordinateur partage).
//!
//! Le code est stocke hache (Argon2) dans `lock.json`. Pendant le verrouillage, la fenetre
//! principale reste chargee mais cachee ; l'ecran de verrouillage est une fenetre separee, servie
//! par le protocole local `lockscreen` (fonctionne donc hors ligne). Ce n'est pas un chiffrement :
//! quiconque accede aux fichiers du compte systeme peut supprimer `lock.json`.

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use tauri::{Manager, http};

use crate::storage::{self, LoadResult};

#[cfg(windows)]
mod hello;

const LOCK_FILE: &str = "lock.json";
const LOCK_WINDOW: &str = "lock";
pub const PROTOCOL: &str = "lockscreen";
/// Fichiers de la page locale (chemin, type, contenu), embarques dans le binaire.
const ASSETS: &[(&str, &str, &[u8])] = &[
    ("/", "text/html; charset=utf-8", include_bytes!("../lockscreen/index.html")),
    ("/logo.png", "image/png", include_bytes!("../icons/128x128@2x.png")),
    (
        "/fonts/inter-latin-400-normal.woff2",
        "font/woff2",
        include_bytes!("../lockscreen/fonts/inter-latin-400-normal.woff2"),
    ),
    (
        "/fonts/inter-latin-500-normal.woff2",
        "font/woff2",
        include_bytes!("../lockscreen/fonts/inter-latin-500-normal.woff2"),
    ),
    (
        "/fonts/inter-latin-600-normal.woff2",
        "font/woff2",
        include_bytes!("../lockscreen/fonts/inter-latin-600-normal.woff2"),
    ),
    (
        "/fonts/inter-latin-700-normal.woff2",
        "font/woff2",
        include_bytes!("../lockscreen/fonts/inter-latin-700-normal.woff2"),
    ),
];
const DEFAULT_TRAY_TEXT: &str = "Ma Journée";
const LOCKED_TRAY_TEXT: &str = "Ma Journée (verrouillée)";
const MIN_CODE_CHARS: usize = 4;
const MAX_CODE_CHARS: usize = 128;
const UNREADABLE: &str = "Les réglages du verrou sont illisibles : utilisez « Code oublié ? ».";
const WATCHDOG_TICK: Duration = Duration::from_secs(15);
const MAIN_WINDOW: &str = "main";

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LockConfig {
    pub enabled: bool,
    /// Hachage Argon2 au format PHC (sel et parametres inclus).
    pub code_hash: Option<String>,
    #[serde(default)]
    pub hello_enabled: bool,
}

#[derive(Default)]
pub struct LockState {
    config: LockConfig,
    /// `lock.json` illisible ou incoherent : verrouille, seule la reinitialisation est proposee.
    unreadable: bool,
    locked: bool,
    /// Une verification de code est en cours : refuse les essais paralleles (contournement du delai).
    verifying: bool,
    failures: u32,
    retry_at: Option<Instant>,
    /// Dernier statut envoye par le site, reaffiche dans le tray au deverrouillage.
    tray_text: Option<String>,
}

pub type SharedLock = Arc<Mutex<LockState>>;

impl LockState {
    fn from_load(result: LoadResult<LockConfig>) -> Self {
        match result {
            // Active sans code : impossible a deverrouiller, traite comme illisible.
            LoadResult::Loaded(config) if config.enabled && config.code_hash.is_none() => Self::unreadable(),
            LoadResult::Loaded(config) => Self { locked: config.enabled, config, ..Default::default() },
            LoadResult::Missing => Self::default(),
            LoadResult::Corrupt => Self::unreadable(),
        }
    }

    fn unreadable() -> Self {
        Self { unreadable: true, locked: true, ..Default::default() }
    }

    fn is_active(&self) -> bool {
        self.config.enabled || self.unreadable
    }

    fn retry_in(&self) -> Duration {
        self.retry_at.map(|t| t.saturating_duration_since(Instant::now())).unwrap_or_default()
    }
}

/// Charge `lock.json` : l'app demarre verrouillee si le verrou est actif.
pub fn load(app: &tauri::AppHandle) -> SharedLock {
    let state = match app.path().app_config_dir() {
        Ok(dir) => LockState::from_load(storage::load_json(&dir.join(LOCK_FILE))),
        Err(_) => LockState::default(),
    };
    if state.locked {
        log::info!("Verrouillage actif au demarrage");
    }
    Arc::new(Mutex::new(state))
}

fn hash_code(code: &str) -> Result<String, String> {
    // Sel aleatoire genere par argon2 (source d'entropie du systeme).
    Argon2::default().hash_password(code.as_bytes()).map(|h| h.to_string()).map_err(|e| e.to_string())
}

fn verify_code(code: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|h| Argon2::default().verify_password(code.as_bytes(), &h).is_ok())
}

fn validate_code(code: &str) -> Result<(), String> {
    let len = code.chars().count();
    if len < MIN_CODE_CHARS {
        Err(format!("Le code doit contenir au moins {MIN_CODE_CHARS} caractères."))
    } else if len > MAX_CODE_CHARS {
        Err("Le code est trop long.".to_string())
    } else {
        Ok(())
    }
}

/// Attente imposee apres `failures` echecs consecutifs : 5 essais libres, puis 30 s doublees a
/// chaque echec, plafonnees a 5 min.
fn failure_delay(failures: u32) -> Duration {
    const FREE_ATTEMPTS: u32 = 5;
    if failures <= FREE_ATTEMPTS {
        return Duration::ZERO;
    }
    let doublings = (failures - FREE_ATTEMPTS - 1).min(4);
    Duration::from_secs((30u64 << doublings).min(300))
}

/// `thread::sleep` est fige pendant la mise en veille : un ecart d'horloge bien superieur au pas
/// attendu signale un reveil.
fn woke_from_sleep(wall_elapsed: Duration, expected: Duration) -> bool {
    wall_elapsed > expected + Duration::from_secs(60)
}

/// Titre et texte a afficher : generiques tant que l'app est verrouillee (rappels personnels,
/// mails, planning...).
fn masked_notification<'a>(locked: bool, title: &'a str, body: &'a str) -> (&'a str, &'a str) {
    if locked { ("Ma Journée", "Nouvelle notification") } else { (title, body) }
}

fn state(app: &tauri::AppHandle) -> SharedLock {
    app.state::<SharedLock>().inner().clone()
}

pub fn is_locked(app: &tauri::AppHandle) -> bool {
    app.try_state::<SharedLock>().is_some_and(|s| s.lock().unwrap().locked)
}

pub fn mask_notification<'a>(app: &tauri::AppHandle, title: &'a str, body: &'a str) -> (&'a str, &'a str) {
    masked_notification(is_locked(app), title, body)
}

fn tray_text(state: &LockState) -> String {
    if state.locked {
        LOCKED_TRAY_TEXT.to_string()
    } else {
        state.tray_text.clone().unwrap_or_else(|| DEFAULT_TRAY_TEXT.to_string())
    }
}

/// Statut du tray envoye par le site : memorise, mais masque tant que l'app est verrouillee.
pub fn set_tray_status(app: &tauri::AppHandle, text: String) -> Result<(), String> {
    let shown = {
        let st = state(app);
        let mut s = st.lock().unwrap();
        s.tray_text = Some(text);
        tray_text(&s)
    };
    app.state::<crate::TrayStatusItem>().0.set_text(shown).map_err(|e| e.to_string())
}

fn refresh_tray(app: &tauri::AppHandle) {
    let shown = tray_text(&state(app).lock().unwrap());
    if let Some(item) = app.try_state::<crate::TrayStatusItem>() {
        let _ = item.0.set_text(shown);
    }
}

/// Verrouille l'app si le verrou est actif. Retourne `true` si elle vient d'etre verrouillee.
pub fn lock_now(app: &tauri::AppHandle) -> bool {
    {
        let st = state(app);
        let mut s = st.lock().unwrap();
        if !s.is_active() || s.locked {
            return false;
        }
        s.locked = true;
    }
    log::info!("Application verrouillee");
    // Releve le theme courant du site pour l'ecran de verrouillage (voir `set_appearance`).
    if let Some(main) = app.get_webview_window(MAIN_WINDOW) {
        let _ = main.eval(crate::REPORT_APPEARANCE_JS);
    }
    refresh_tray(app);
    // Ecran de reglages ouvert : bascule sur le deverrouillage.
    if app.get_webview_window(LOCK_WINDOW).is_some() {
        show_window(app, Mode::Unlock);
    }
    true
}

/// Au reveil, la fenetre principale peut etre restee affichee : elle est cachee derriere l'ecran de
/// verrouillage.
fn lock_after_sleep(app: &tauri::AppHandle) {
    if !lock_now(app) {
        return;
    }
    if let Some(main) = app.get_webview_window(MAIN_WINDOW)
        && main.is_visible().unwrap_or(false)
    {
        let _ = main.hide();
        show_window(app, Mode::Unlock);
    }
}

pub fn start_sleep_watchdog(app: tauri::AppHandle) {
    thread::spawn(move || {
        let mut last = SystemTime::now();
        loop {
            thread::sleep(WATCHDOG_TICK);
            let now = SystemTime::now();
            if woke_from_sleep(now.duration_since(last).unwrap_or_default(), WATCHDOG_TICK) {
                log::info!("Reveil apres mise en veille detecte");
                lock_after_sleep(&app);
            }
            last = now;
        }
    });
}

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Unlock,
    Setup,
}

/// Racine du protocole selon la convention de wry (`http://<schema>.localhost` sous Windows).
fn page_base() -> String {
    if cfg!(windows) { format!("http://{PROTOCOL}.localhost/") } else { format!("{PROTOCOL}://localhost/") }
}

pub fn serve_asset(request: &http::Request<Vec<u8>>) -> http::Response<Cow<'static, [u8]>> {
    let response = match ASSETS.iter().find(|(path, ..)| *path == request.uri().path()) {
        Some((_, mime, body)) => {
            http::Response::builder().header(http::header::CONTENT_TYPE, *mime).body(Cow::Borrowed(*body))
        }
        None => http::Response::builder().status(http::StatusCode::NOT_FOUND).body(Cow::Borrowed(&[][..])),
    };
    response.expect("en-tetes statiques valides")
}

/// Affiche l'ecran de verrouillage (ou de reglage du verrou), en le creant si besoin.
pub fn show_window(app: &tauri::AppHandle, mode: Mode) {
    let (query, title) = match mode {
        Mode::Unlock => ("?mode=unlock", "Ma Journée verrouillée"),
        Mode::Setup => ("?mode=setup", "Verrouillage de Ma Journée"),
    };
    let base = page_base();
    let url: tauri::Url = format!("{base}{query}").parse().expect("URL locale valide");

    if let Some(w) = app.get_webview_window(LOCK_WINDOW) {
        if w.url().ok().as_ref() != Some(&url) {
            let _ = w.navigate(url);
            let _ = w.set_title(title);
        }
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }

    let result = tauri::WebviewWindowBuilder::new(app, LOCK_WINDOW, tauri::WebviewUrl::CustomProtocol(url))
        .title(title)
        .inner_size(440.0, 660.0)
        .resizable(false)
        .maximizable(false)
        .center()
        .focused(true)
        // Page locale uniquement : tout lien eventuel est ignore.
        .on_navigation(move |url| url.as_str().starts_with(&base))
        .build();
    if let Err(e) = result {
        log::error!("Echec ouverture de l'ecran de verrouillage : {}", e);
    }
}

/// Reglages du verrou (menu Fichier, ou site) : ecran de deverrouillage si l'app est verrouillee.
pub fn open_settings(app: &tauri::AppHandle) {
    show_window(app, if is_locked(app) { Mode::Unlock } else { Mode::Setup });
}

fn unlock(app: &tauri::AppHandle, st: &SharedLock) {
    st.lock().unwrap().locked = false;
    log::info!("Application deverrouillee");
    refresh_tray(app);
    if let Some(w) = app.get_webview_window(LOCK_WINDOW) {
        let _ = w.close();
    }
    crate::show_and_focus_main_window(app);
}

/// Les commandes qui modifient le verrou ne repondent qu'a l'ecran de verrouillage : le label de la
/// fenetre appelante est fixe par Tauri, la page distante ne peut donc ni deverrouiller, ni
/// desactiver le verrou, ni lancer la reinitialisation.
fn require_lock_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    if window.label() == LOCK_WINDOW {
        Ok(())
    } else {
        log::warn!("Commande de verrouillage refusee depuis la fenetre '{}'", window.label());
        Err("Action réservée à l'écran de verrouillage.".to_string())
    }
}

fn wait_message(wait: Duration) -> String {
    format!("Trop d'essais. Réessayez dans {} s.", wait.as_millis().div_ceil(1000))
}

/// Verifie `code` contre le code enregistre, avec un delai croissant apres des echecs repetes.
async fn check_code(st: &SharedLock, code: String) -> Result<(), String> {
    let hash = {
        let mut s = st.lock().unwrap();
        let wait = s.retry_in();
        if !wait.is_zero() {
            return Err(wait_message(wait));
        }
        if s.verifying {
            return Err("Vérification déjà en cours.".to_string());
        }
        let hash = s.config.code_hash.clone().ok_or("Aucun code n'est défini.")?;
        s.verifying = true;
        hash
    };
    let ok = tauri::async_runtime::spawn_blocking(move || verify_code(&code, &hash)).await.unwrap_or(false);
    let mut s = st.lock().unwrap();
    s.verifying = false;
    if ok {
        s.failures = 0;
        s.retry_at = None;
        return Ok(());
    }
    s.failures += 1;
    let delay = failure_delay(s.failures);
    s.retry_at = (!delay.is_zero()).then(|| Instant::now() + delay);
    log::warn!("Code de verrouillage incorrect ({} echec(s) consecutif(s))", s.failures);
    if delay.is_zero() { Err("Code incorrect.".to_string()) } else { Err(wait_message(delay)) }
}

async fn hello_available() -> bool {
    #[cfg(windows)]
    {
        tauri::async_runtime::spawn_blocking(hello::is_available).await.unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(windows)]
async fn hello_verify(window: &tauri::WebviewWindow) -> Result<bool, String> {
    let hwnd = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
    tauri::async_runtime::spawn_blocking(move || hello::verify(hwnd, "Déverrouiller Ma Journée"))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(not(windows))]
async fn hello_verify(_window: &tauri::WebviewWindow) -> Result<bool, String> {
    Err("Windows Hello n'est disponible que sous Windows.".to_string())
}

#[derive(serde::Serialize)]
pub struct LockStatus {
    enabled: bool,
    locked: bool,
    unreadable: bool,
    hello_available: bool,
    hello_enabled: bool,
    retry_in_secs: u64,
    /// Apparence du site (theme, couleur d'accent), reprise par l'ecran de verrouillage.
    theme: String,
    accent: String,
}

#[tauri::command]
pub async fn lock_status(app: tauri::AppHandle) -> Result<LockStatus, String> {
    let hello_available = hello_available().await;
    let appearance = app.state::<crate::SharedAppSettings>().lock().unwrap().appearance.clone();
    let st = state(&app);
    let s = st.lock().unwrap();
    Ok(LockStatus {
        enabled: s.config.enabled,
        locked: s.locked,
        unreadable: s.unreadable,
        hello_available,
        hello_enabled: s.config.hello_enabled,
        retry_in_secs: s.retry_in().as_millis().div_ceil(1000) as u64,
        theme: appearance.theme,
        accent: appearance.accent,
    })
}

#[tauri::command]
pub async fn lock_unlock_with_code(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    code: String,
) -> Result<(), String> {
    require_lock_window(&window)?;
    let st = state(&app);
    {
        let s = st.lock().unwrap();
        if s.unreadable {
            return Err(UNREADABLE.to_string());
        }
        if !s.locked {
            return Ok(());
        }
    }
    check_code(&st, code).await?;
    unlock(&app, &st);
    Ok(())
}

#[tauri::command]
pub async fn lock_unlock_with_hello(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    require_lock_window(&window)?;
    let st = state(&app);
    {
        let s = st.lock().unwrap();
        if s.unreadable {
            return Err(UNREADABLE.to_string());
        }
        if !s.config.hello_enabled {
            return Err("Windows Hello n'est pas activé.".to_string());
        }
        if !s.locked {
            return Ok(());
        }
    }
    if !hello_verify(&window).await? {
        return Err("Vérification Windows Hello annulée ou refusée.".to_string());
    }
    unlock(&app, &st);
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct LockSettings {
    enabled: bool,
    current_code: Option<String>,
    new_code: Option<String>,
    hello_enabled: bool,
}

/// Active, modifie ou desactive le verrou. Le code actuel est exige si le verrou est deja actif.
#[tauri::command]
pub async fn lock_configure(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    settings: LockSettings,
) -> Result<(), String> {
    require_lock_window(&window)?;
    let st = state(&app);
    let was_enabled = {
        let s = st.lock().unwrap();
        if s.locked || s.unreadable {
            return Err("Déverrouillez d'abord l'application.".to_string());
        }
        s.config.enabled
    };
    if was_enabled {
        check_code(&st, settings.current_code.unwrap_or_default()).await?;
    }

    let config = if settings.enabled {
        let code_hash = match settings.new_code.filter(|c| !c.is_empty()) {
            Some(code) => {
                validate_code(&code)?;
                Some(
                    tauri::async_runtime::spawn_blocking(move || hash_code(&code))
                        .await
                        .map_err(|e| e.to_string())??,
                )
            }
            None => st.lock().unwrap().config.code_hash.clone(),
        };
        if code_hash.is_none() {
            return Err("Choisissez un code.".to_string());
        }
        LockConfig { enabled: true, code_hash, hello_enabled: settings.hello_enabled && cfg!(windows) }
    } else {
        LockConfig::default()
    };

    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    storage::save_json(&dir.join(LOCK_FILE), &config).inspect_err(|e| {
        log::error!("Echec sauvegarde du verrouillage : {}", e);
    })?;
    log::info!("Verrouillage {}", if config.enabled { "active" } else { "desactive" });
    st.lock().unwrap().config = config;
    Ok(())
}

/// Code oublie : supprime le verrou et efface les donnees de navigation de l'app (cookies,
/// stockage local), ce qui deconnecte le compte. Le contenu reste ainsi protege.
#[tauri::command]
pub async fn lock_reset(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    require_lock_window(&window)?;
    let st = state(&app);
    if !st.lock().unwrap().locked {
        return Err("L'application n'est pas verrouillée.".to_string());
    }
    let main = app.get_webview_window(MAIN_WINDOW).ok_or("Fenêtre principale introuvable.")?;
    main.clear_all_browsing_data().map_err(|e| {
        log::error!("Echec effacement des donnees de navigation : {}", e);
        e.to_string()
    })?;
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    match std::fs::remove_file(dir.join(LOCK_FILE)) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            log::error!("Echec suppression de {} : {}", LOCK_FILE, e);
            return Err(e.to_string());
        }
    }
    *st.lock().unwrap() = LockState { locked: true, ..Default::default() };
    log::warn!("Verrouillage reinitialise, donnees de navigation effacees");
    let _ = main.navigate(crate::FRONTEND.1.parse().expect("URL du site valide"));
    unlock(&app, &st);
    Ok(())
}

#[tauri::command]
pub async fn open_lock_settings(app: tauri::AppHandle) {
    open_settings(&app);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── hash_code() / verify_code() / validate_code() ─────────────────────────

    #[test]
    fn hashed_code_verifies_and_wrong_code_is_rejected() {
        let hash = hash_code("1234").unwrap();
        assert!(!hash.contains("1234"));
        assert!(verify_code("1234", &hash));
        assert!(!verify_code("1235", &hash));
        assert!(!verify_code("", &hash));
    }

    #[test]
    fn same_code_gets_a_different_salt_each_time() {
        assert_ne!(hash_code("1234").unwrap(), hash_code("1234").unwrap());
    }

    #[test]
    fn hash_created_by_argon2_0_5_still_verifies() {
        // Code "1234" hache par la version precedente d'argon2 : le format PHC est stable.
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$ZI8cJBe5FGsyPy7aPAcQYw$xkqvbFl1AaPLmNbN+J18REIA+cTbnlrPIAtTZBML2g4";
        assert!(verify_code("1234", hash));
        assert!(!verify_code("1235", hash));
    }

    #[test]
    fn page_assets_are_served_and_unknown_paths_are_not_found() {
        let get = |path: &str| serve_asset(&http::Request::get(path).body(Vec::new()).unwrap());
        let page = get("/?mode=unlock");
        assert_eq!(page.status(), http::StatusCode::OK);
        assert!(page.headers()[http::header::CONTENT_TYPE].to_str().unwrap().starts_with("text/html"));
        assert_eq!(get("/fonts/inter-latin-400-normal.woff2").headers()[http::header::CONTENT_TYPE], "font/woff2");
        assert_eq!(get("/logo.png").status(), http::StatusCode::OK);
        assert_eq!(get("/../lock.json").status(), http::StatusCode::NOT_FOUND);
        assert_eq!(get("/inconnu").status(), http::StatusCode::NOT_FOUND);
    }

    #[test]
    fn malformed_hash_never_verifies() {
        assert!(!verify_code("1234", "pas un hachage"));
    }

    #[test]
    fn code_length_limits() {
        assert!(validate_code("123").is_err());
        assert!(validate_code("1234").is_ok());
        // Compte en caracteres, pas en octets.
        assert!(validate_code("éééé").is_ok());
        assert!(validate_code(&"a".repeat(MAX_CODE_CHARS)).is_ok());
        assert!(validate_code(&"a".repeat(MAX_CODE_CHARS + 1)).is_err());
    }

    // ── failure_delay() ───────────────────────────────────────────────────────

    #[test]
    fn first_five_failures_are_free_then_delay_doubles_up_to_five_minutes() {
        for n in 0..=5 {
            assert_eq!(failure_delay(n), Duration::ZERO, "{n}");
        }
        assert_eq!(failure_delay(6), Duration::from_secs(30));
        assert_eq!(failure_delay(7), Duration::from_secs(60));
        assert_eq!(failure_delay(8), Duration::from_secs(120));
        assert_eq!(failure_delay(9), Duration::from_secs(240));
        assert_eq!(failure_delay(10), Duration::from_secs(300));
        assert_eq!(failure_delay(u32::MAX), Duration::from_secs(300));
    }

    // ── woke_from_sleep() ─────────────────────────────────────────────────────

    #[test]
    fn regular_tick_is_not_a_wake_up() {
        assert!(!woke_from_sleep(Duration::from_secs(15), WATCHDOG_TICK));
        assert!(!woke_from_sleep(Duration::from_secs(70), WATCHDOG_TICK));
    }

    #[test]
    fn long_gap_is_a_wake_up() {
        assert!(woke_from_sleep(Duration::from_secs(76), WATCHDOG_TICK));
        assert!(woke_from_sleep(Duration::from_secs(3600), WATCHDOG_TICK));
    }

    // ── masked_notification() ─────────────────────────────────────────────────

    #[test]
    fn notification_content_is_hidden_only_while_locked() {
        assert_eq!(masked_notification(false, "Titre", "Texte"), ("Titre", "Texte"));
        let (title, body) = masked_notification(true, "Titre", "Texte");
        assert!(!title.contains("Titre") && !body.contains("Texte"));
    }

    // ── LockState::from_load() ────────────────────────────────────────────────

    fn enabled_config() -> LockConfig {
        LockConfig { enabled: true, code_hash: Some(hash_code("1234").unwrap()), hello_enabled: false }
    }

    #[test]
    fn missing_file_means_no_lock() {
        let s = LockState::from_load(LoadResult::Missing);
        assert!(!s.locked && !s.is_active());
    }

    #[test]
    fn enabled_lock_starts_locked() {
        let s = LockState::from_load(LoadResult::Loaded(enabled_config()));
        assert!(s.locked && !s.unreadable);
    }

    #[test]
    fn disabled_lock_starts_unlocked() {
        let s = LockState::from_load(LoadResult::Loaded(LockConfig::default()));
        assert!(!s.locked && !s.is_active());
    }

    #[test]
    fn corrupt_or_inconsistent_file_fails_closed() {
        for result in [
            LoadResult::Corrupt,
            LoadResult::Loaded(LockConfig { enabled: true, code_hash: None, hello_enabled: false }),
        ] {
            let s = LockState::from_load(result);
            assert!(s.locked && s.unreadable && s.is_active());
        }
    }

    #[test]
    fn lock_config_round_trips_on_disk() {
        let tmp = storage::TempDir::new("lock");
        let path = tmp.0.join(LOCK_FILE);
        let config = enabled_config();
        storage::save_json(&path, &config).unwrap();
        assert!(matches!(storage::load_json::<LockConfig>(&path), LoadResult::Loaded(c) if c == config));
    }

    // ── tray_text() ───────────────────────────────────────────────────────────

    #[test]
    fn tray_status_is_masked_while_locked_and_restored_after() {
        let mut s = LockState { tray_text: Some("3 tâches restantes".to_string()), ..Default::default() };
        assert_eq!(tray_text(&s), "3 tâches restantes");
        s.locked = true;
        assert_eq!(tray_text(&s), LOCKED_TRAY_TEXT);
        s.tray_text = None;
        s.locked = false;
        assert_eq!(tray_text(&s), DEFAULT_TRAY_TEXT);
    }
}
