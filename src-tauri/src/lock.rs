//! Verrouillage local de l'app (ecran de confidentialite sur un ordinateur partage).
//!
//! Le code est stocke hache (Argon2) dans `lock.json` ; etat, declencheurs, verification et
//! masquage restent ici. L'ecran lui-meme est affiche par le site dans la fenetre principale, qui
//! reste cachee tant qu'il ne l'a pas signale (`lock_screen_ready`). Sinon (site injoignable,
//! ancienne version, pas de reponse sous `WEB_LOCK_TIMEOUT`), une page locale de secours s'ouvre
//! dans une fenetre separee, servie par le protocole `lockscreen`. Ce n'est pas un chiffrement :
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
use tauri::{Emitter, Manager, http};

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
/// Delai laisse au site pour afficher son ecran avant d'ouvrir la page locale de secours.
const WEB_LOCK_TIMEOUT: Duration = Duration::from_secs(3);
const MAIN_WINDOW: &str = "main";
/// Evenement envoye a la fenetre principale a chaque verrouillage ou deverrouillage.
const LOCK_EVENT: &str = "lock-state-changed";

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
    /// La page chargee gere l'ecran de verrouillage et ses reglages (`lock_web_register`).
    web_supported: bool,
    /// L'ecran de verrouillage du site est affiche : la fenetre principale peut apparaitre.
    overlay_shown: bool,
    /// Affichage demande pendant le verrouillage, en attente de l'ecran du site.
    pending_show: bool,
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

    /// Affichage demande alors que l'app est verrouillee : `true` si l'ecran du site est deja
    /// affiche (montrer la fenetre principale), sinon l'affichage est mis en attente.
    fn request_show(&mut self) -> bool {
        self.pending_show = !self.overlay_shown;
        self.overlay_shown
    }

    /// Fin du delai d'attente : `true` s'il faut ouvrir la page locale de secours.
    fn take_pending(&mut self) -> bool {
        let fallback = self.pending_show && self.locked;
        self.pending_show = false;
        fallback
    }

    /// Le site a affiche son ecran. `None` si l'app n'est pas verrouillee (signal ignore), sinon
    /// `Some(true)` si un affichage etait en attente.
    fn overlay_ready(&mut self) -> Option<bool> {
        if !self.locked {
            return None;
        }
        self.overlay_shown = true;
        Some(std::mem::take(&mut self.pending_show))
    }

    /// Nouvelle page dans la fenetre principale : elle devra s'annoncer de nouveau.
    fn page_started(&mut self) {
        self.web_supported = false;
        self.overlay_shown = false;
    }

    fn set_locked(&mut self, locked: bool) {
        self.locked = locked;
        self.overlay_shown = false;
        self.pending_show = false;
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
        s.set_locked(true);
    }
    log::info!("Application verrouillee");
    emit_lock_state(app, true);
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
        show_locked(app);
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

#[derive(Clone, serde::Serialize)]
struct LockChanged {
    locked: bool,
}

fn emit_lock_state(app: &tauri::AppHandle, locked: bool) {
    let _ = app.emit_to(MAIN_WINDOW, LOCK_EVENT, LockChanged { locked });
}

/// L'app verrouillee doit apparaitre (tray, notification, second lancement...) : fenetre
/// principale si l'ecran du site est affiche, sinon attente de celui-ci puis page de secours.
pub fn show_locked(app: &tauri::AppHandle) {
    if state(app).lock().unwrap().request_show() {
        crate::focus_main_window(app);
        return;
    }
    let app = app.clone();
    thread::spawn(move || {
        thread::sleep(WEB_LOCK_TIMEOUT);
        if state(&app).lock().unwrap().take_pending() {
            log::warn!("Ecran de verrouillage du site indisponible, page locale affichee");
            show_window(&app, Mode::Unlock);
        }
    });
}

/// Appele au debut de chaque chargement de page dans la fenetre principale.
pub fn on_main_page_started(app: &tauri::AppHandle) {
    if let Some(st) = app.try_state::<SharedLock>() {
        st.lock().unwrap().page_started();
    }
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

/// Reglages du verrou (menu Fichier) : dans le site s'il les gere, sinon page locale. Ecran de
/// deverrouillage si l'app est verrouillee.
pub fn open_settings(app: &tauri::AppHandle) {
    let (locked, web_supported) = {
        let st = state(app);
        let s = st.lock().unwrap();
        (s.locked, s.web_supported)
    };
    if locked {
        show_locked(app);
    } else if web_supported {
        crate::focus_main_window(app);
        let _ = app.emit_to(MAIN_WINDOW, "native-menu", "lockSettings");
    } else {
        show_window(app, Mode::Setup);
    }
}

fn unlock(app: &tauri::AppHandle, st: &SharedLock) {
    st.lock().unwrap().set_locked(false);
    log::info!("Application deverrouillee");
    emit_lock_state(app, false);
    refresh_tray(app);
    if let Some(w) = app.get_webview_window(LOCK_WINDOW) {
        let _ = w.close();
    }
    crate::show_and_focus_main_window(app);
}

/// Fenetres autorisees a piloter le verrou : la principale (ecran du site) et la page locale de
/// secours. Le label est fixe par Tauri. Le site est de confiance : compromis, il verrait de toute
/// facon les donnees ; le verrou protege contre une personne devant l'ordinateur.
fn require_lock_ui(window: &tauri::WebviewWindow) -> Result<(), String> {
    if window.label() == MAIN_WINDOW || window.label() == LOCK_WINDOW {
        Ok(())
    } else {
        log::warn!("Commande de verrouillage refusee depuis la fenetre '{}'", window.label());
        Err("Action réservée à l'écran de verrouillage.".to_string())
    }
}

fn require_main_window(window: &tauri::WebviewWindow) -> Result<(), String> {
    if window.label() == MAIN_WINDOW { Ok(()) } else { Err("Action réservée à la fenêtre principale.".to_string()) }
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
    require_lock_ui(&window)?;
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
    require_lock_ui(&window)?;
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
    require_lock_ui(&window)?;
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
    require_lock_ui(&window)?;
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

/// Le site annonce qu'il gere l'ecran de verrouillage et ses reglages (a chaque chargement de
/// page, le plus tot possible). Retourne l'etat courant.
#[tauri::command]
pub async fn lock_web_register(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<LockStatus, String> {
    require_main_window(&window)?;
    state(&app).lock().unwrap().web_supported = true;
    lock_status(app).await
}

/// Le site signale que son ecran de verrouillage est affiche (et le reste de la page inerte) : la
/// fenetre principale peut apparaitre, a la place de la page locale de secours si elle est ouverte.
#[tauri::command]
pub fn lock_screen_ready(app: tauri::AppHandle, window: tauri::WebviewWindow) -> Result<(), String> {
    require_main_window(&window)?;
    let Some(pending) = state(&app).lock().unwrap().overlay_ready() else {
        return Ok(());
    };
    if let Some(fallback) = app.get_webview_window(LOCK_WINDOW) {
        let _ = fallback.close();
        crate::focus_main_window(&app);
    } else if pending {
        crate::focus_main_window(&app);
    }
    Ok(())
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

    // ── Affichage pendant le verrouillage (ecran du site / page de secours) ───

    fn locked_state() -> LockState {
        LockState::from_load(LoadResult::Loaded(enabled_config()))
    }

    #[test]
    fn show_waits_for_the_site_screen_then_falls_back() {
        let mut s = locked_state();
        assert!(!s.request_show());
        assert!(s.take_pending());
        // Une seule page de secours par demande.
        assert!(!s.take_pending());
    }

    #[test]
    fn site_screen_ready_shows_the_main_window() {
        let mut s = locked_state();
        assert!(!s.request_show());
        assert_eq!(s.overlay_ready(), Some(true));
        assert!(!s.take_pending());
        // Ecran deja affiche : les demandes suivantes montrent directement la fenetre.
        assert!(s.request_show());
    }

    #[test]
    fn ready_signal_is_ignored_when_unlocked() {
        let mut s = LockState::default();
        assert_eq!(s.overlay_ready(), None);
        assert!(!s.overlay_shown);
    }

    #[test]
    fn new_page_or_new_lock_must_show_the_site_screen_again() {
        let mut s = locked_state();
        s.web_supported = true;
        s.overlay_ready();
        s.page_started();
        assert!(!s.web_supported && !s.request_show());

        let mut s = locked_state();
        s.overlay_ready();
        s.set_locked(false);
        s.set_locked(true);
        assert!(!s.request_show());
    }

    #[test]
    fn unlocking_cancels_a_pending_fallback() {
        let mut s = locked_state();
        s.request_show();
        s.set_locked(false);
        assert!(!s.take_pending());
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
