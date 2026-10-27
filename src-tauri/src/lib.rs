use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};
use tauri::{
    Emitter, Manager,
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
};

mod lock;
mod storage;
use storage::LoadResult;

/// Notification programmee par l'utilisateur (titre, message, heure, jours de la
/// semaine, periode de validite optionnelle). Equivalent bureau de
/// `CustomNotification.kt` cote Android, pour une parite de fonctionnalite.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct CustomNotification {
    pub id: String,
    pub label: String,
    pub message: String,
    pub time: String,   // "HH:MM"
    pub days: Vec<u32>, // 1=lundi..7=dimanche (convention ISO, identique a Android)
    pub enabled: bool,
    pub start_date: Option<String>, // "YYYY-MM-DD"
    pub end_date: Option<String>,   // "YYYY-MM-DD"
}

/// Format persiste avant l'introduction des notifications personnalisees,
/// conserve uniquement pour la migration automatique (voir `load_notifications`).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
struct LegacyWeeklyNotifConfig {
    enabled: bool,
    day_of_week: u32, // 0=dimanche..6=samedi (convention JS)
    hour: u32,
    minute: u32,
}

type SharedNotifications = Arc<Mutex<Vec<CustomNotification>>>;
type PendingUpdateVersion = Arc<Mutex<Option<String>>>;

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct DesktopAppSettings {
    pub update_notif_enabled: bool,
    #[serde(default)]
    pub tray_hint_shown: bool,
}

impl Default for DesktopAppSettings {
    fn default() -> Self {
        Self { update_notif_enabled: true, tray_hint_shown: false }
    }
}

type SharedAppSettings = Arc<Mutex<DesktopAppSettings>>;

/// Handle vers l'item de menu (non cliquable) affichant le statut du jour dans le
/// menu clic droit du tray, pour pouvoir le mettre a jour depuis une commande
/// appelee bien apres sa creation dans `setup`.
struct TrayStatusItem(MenuItem<tauri::Wry>);

#[tauri::command]
fn update_tray_status(app: tauri::AppHandle, text: String) -> Result<(), String> {
    lock::set_tray_status(&app, text)
}

#[tauri::command]
fn get_pending_update(state: tauri::State<'_, PendingUpdateVersion>) -> Option<String> {
    state.lock().unwrap().clone()
}

#[tauri::command]
fn get_update_notif_enabled(state: tauri::State<'_, SharedAppSettings>) -> bool {
    state.lock().unwrap().update_notif_enabled
}

#[tauri::command]
fn set_update_notif_enabled(
    state: tauri::State<'_, SharedAppSettings>,
    app: tauri::AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = state.lock().unwrap();
    let updated = DesktopAppSettings { update_notif_enabled: enabled, ..settings.clone() };
    save_app_settings(&app, &updated).inspect_err(|e| {
        log::error!("Echec sauvegarde des reglages de l'app : {}", e);
    })?;
    *settings = updated;
    log::info!("Notification de mise a jour {}", if enabled { "activee" } else { "desactivee" });
    Ok(())
}

/// Empeche deux installations simultanees (ex: double clic sur "Installer").
static UPDATE_INSTALLING: AtomicBool = AtomicBool::new(false);

/// Libere `UPDATE_INSTALLING` si l'installation echoue (sortie anticipee par `?`).
struct UpdateInstallGuard;

impl Drop for UpdateInstallGuard {
    fn drop(&mut self) {
        UPDATE_INSTALLING.store(false, Ordering::SeqCst);
    }
}

/// Palier de progression du telechargement : le pourcentage si la taille totale est connue,
/// sinon le nombre de Mo recus. Un evenement n'est emis qu'a chaque changement de palier.
fn progress_step(downloaded: u64, total: Option<u64>) -> u64 {
    match total {
        Some(total) if total > 0 => downloaded.min(total) * 100 / total,
        _ => downloaded / 1_000_000,
    }
}

#[derive(Clone, serde::Serialize)]
struct UpdateDownloadProgress {
    downloaded: u64,
    total: Option<u64>,
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    if UPDATE_INSTALLING.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        log::warn!("Installation de mise a jour deja en cours, demande ignoree");
        return Err("Installation deja en cours".to_string());
    }
    let _guard = UpdateInstallGuard;
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| {
            log::error!("Echec check() pendant install_update: {}", e);
            e.to_string()
        })?
        .ok_or_else(|| "Aucune mise a jour disponible".to_string())?;
    log::info!("Installation de la mise a jour v{}", update.version);
    // Progression relayee au frontend (evenement "update-download-progress"), comme "native-menu".
    let downloaded = AtomicU64::new(0);
    let total = Mutex::new(None);
    let last_step = AtomicU64::new(u64::MAX);
    let emit_progress = |downloaded, total| {
        let _ = app.emit("update-download-progress", UpdateDownloadProgress { downloaded, total });
    };
    update
        .download_and_install(
            |chunk, content_length| {
                let now = downloaded.fetch_add(chunk as u64, Ordering::Relaxed) + chunk as u64;
                *total.lock().unwrap() = content_length;
                let step = progress_step(now, content_length);
                if last_step.swap(step, Ordering::Relaxed) != step {
                    emit_progress(now, content_length);
                }
            },
            // Dernier evenement, utile surtout quand la taille totale est inconnue.
            || emit_progress(downloaded.load(Ordering::Relaxed), *total.lock().unwrap()),
        )
        .await
        .map_err(|e| {
            log::error!("Echec telechargement/installation: {}", e);
            e.to_string()
        })?;
    app.restart();
}

#[tauri::command]
fn get_app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let autolaunch = app.autolaunch();
    let result = if enabled { autolaunch.enable() } else { autolaunch.disable() };
    match &result {
        Ok(()) => log::info!("Autostart configure (enabled={})", enabled),
        Err(e) => log::error!("Echec configuration autostart (enabled={}): {}", enabled, e),
    }
    result.map_err(|e| e.to_string())
}

/// N'autorise que http/https/mailto. Sans ce garde, une chaine arbitraire (issue du
/// frontend distant) passee a `start`/`xdg-open` pourrait lancer un executable local
/// ou un chemin UNC (ex: `start "" \\host\evil.exe`).
fn is_allowed_url_scheme(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:")
}

enum NavigationAction {
    /// Page du site de l'app : navigation normale dans la fenetre.
    Stay,
    /// Autre site http(s) ou mailto : ouvert dans l'application par defaut du systeme.
    OpenExternally,
    /// Tout le reste (file:, javascript:, data:...) : bloque, jamais transmis a l'OS.
    Block,
}

fn navigation_action(url: &tauri::Url, frontend_host: &str) -> NavigationAction {
    if url.host_str() == Some(frontend_host) {
        NavigationAction::Stay
    } else if is_allowed_url_scheme(url.as_str()) {
        NavigationAction::OpenExternally
    } else {
        NavigationAction::Block
    }
}

#[tauri::command]
fn open_url(url: String) -> Result<(), String> {
    if is_allowed_url_scheme(&url) {
        open_in_browser(&url)
    } else {
        log::warn!("open_url refuse : schema non autorise");
        Err("Schema non autorise".to_string())
    }
}

#[tauri::command]
fn get_platform() -> &'static str {
    #[cfg(target_os = "linux")]
    {
        "linux"
    }
    #[cfg(target_os = "windows")]
    {
        "windows"
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        "other"
    }
}

#[tauri::command]
fn get_custom_notifications(state: tauri::State<'_, SharedNotifications>) -> Vec<CustomNotification> {
    state.lock().unwrap().clone()
}

/// Valide puis sauvegarde la liste envoyee par le frontend. Rien n'est modifie (ni memoire, ni
/// disque) si la liste est invalide ou si l'ecriture echoue : l'erreur remonte au frontend.
#[tauri::command]
fn set_custom_notifications(
    state: tauri::State<'_, SharedNotifications>,
    app: tauri::AppHandle,
    notifications: Vec<CustomNotification>,
) -> Result<(), String> {
    validate_notifications(&notifications).inspect_err(|e| {
        log::warn!("Notifications personnalisees refusees : {}", e);
    })?;
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    // Verrou pris avant l'ecriture : deux appels rapproches ecriraient sinon le meme `.tmp` en
    // parallele, et le disque pourrait finir different de la memoire.
    let mut current = state.lock().unwrap();
    save_notifications_in(&dir, &notifications).inspect_err(|e| {
        log::error!("Echec sauvegarde des notifications personnalisees : {}", e);
    })?;
    // Compte uniquement : titre/message sont du contenu utilisateur, jamais journalises.
    log::info!("Notifications personnalisees sauvegardees ({} entree(s))", notifications.len());
    *current = notifications;
    Ok(())
}

const NOTIFICATIONS_FILE: &str = "custom_notifications.json";
const LEGACY_NOTIFICATIONS_FILE: &str = "weekly_notif.json";
const APP_SETTINGS_FILE: &str = "app_settings.json";

fn config_file(app: &tauri::AppHandle, name: &str) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join(name))
}

fn save_app_settings(app: &tauri::AppHandle, settings: &DesktopAppSettings) -> Result<(), String> {
    let path = config_file(app, APP_SETTINGS_FILE).ok_or("dossier de configuration introuvable")?;
    storage::save_json(&path, settings)
}

fn load_app_settings(app: &tauri::AppHandle) -> DesktopAppSettings {
    match config_file(app, APP_SETTINGS_FILE).map(|p| storage::load_json(&p)) {
        Some(LoadResult::Loaded(settings)) => settings,
        _ => DesktopAppSettings::default(),
    }
}

fn crash_log_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path().app_log_dir().unwrap_or_else(|_| std::env::temp_dir()).join("last_panic.log")
}

/// Si l'app a plante lors de la session precedente (cf. `install_panic_hook`), reverse
/// le detail dans le journal normal au demarrage suivant, pour qu'il soit inclus
/// automatiquement dans un futur "Signaler un probleme" comme le reste du journal
/// technique, sans plomberie supplementaire.
fn report_previous_crash_if_any(app: &tauri::AppHandle) {
    let path = crash_log_path(app);
    if let Ok(content) = std::fs::read_to_string(&path) {
        log::error!("Crash detecte au demarrage precedent :\n{}", content);
        let _ = std::fs::remove_file(&path);
    }
}

/// Installe un hook de panic qui ecrit le detail du crash sur disque avant l'abort.
/// En profil release, `panic = "abort"` (Cargo.toml) : un panic sur n'importe quel
/// thread, y compris les boucles de fond demarrees plus bas, tue tout le processus
/// instantanement sans laisser de trace visible. Le hook s'execute quand meme juste
/// avant l'abort (sur le thread qui panique). Ecriture disque directe plutot que via
/// `log`/tauri-plugin-log : ce dernier peut bufferiser, ce qui ne garantirait pas que
/// le message atteigne le disque avant la mort du processus. Le hook precedent (affichage sur
/// stderr) est rappele ensuite, pour que les panics restent visibles en developpement.
fn install_panic_hook(app: &tauri::AppHandle) {
    let path = crash_log_path(app);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "emplacement inconnu".to_string());
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "message indisponible".to_string());
        let thread = std::thread::current().name().unwrap_or("inconnu").to_string();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let content = format!(
            "Ma Journee v{} - panic sur le thread '{}' a {}\n{}\n\nPile d'appels :\n{}\n",
            env!("CARGO_PKG_VERSION"),
            thread,
            location,
            message,
            backtrace
        );
        let _ = std::fs::write(&path, content);
        previous_hook(info);
    }));
}

/// Logique pure (aucun acces disque) pour rester testable independamment de
/// `clear_webview_cache_if_updated`. `last` peut contenir un retour a la ligne final
/// (lu depuis un fichier ecrit par `std::fs::write`), d'ou le `trim()` cote uniquement.
fn has_version_changed(last: &str, current: &str) -> bool {
    last.trim() != current
}

fn clear_webview_cache_if_updated(app: &tauri::AppHandle) {
    let current = env!("CARGO_PKG_VERSION");
    let config_dir = match app.path().app_config_dir() {
        Ok(d) => d,
        Err(_) => return,
    };
    let marker = config_dir.join("last_version.txt");
    let last = std::fs::read_to_string(&marker).unwrap_or_default();
    if !has_version_changed(&last, current) {
        return;
    }
    if let Ok(data_dir) = app.path().app_data_dir() {
        // WebKitGTK (Linux) : cache HTTP seulement. Ne pas vider CacheStorage (cache du
        // Service Worker, qui gere deja ses versions) : sinon l'app ne demarre plus hors ligne.
        let _ = std::fs::remove_dir_all(data_dir.join("WebKitCache"));
    }
    // WebView2 (Windows) : profil dans app_local_data_dir (AppData\Local), pas dans
    // app_data_dir (Roaming). Ne vider que le cache HTTP et le cache de code JS : le reste
    // du profil contient le cache du Service Worker, localStorage et IndexedDB.
    #[cfg(windows)]
    if let Ok(local_dir) = app.path().app_local_data_dir() {
        let profile = local_dir.join("EBWebView").join("Default");
        let _ = std::fs::remove_dir_all(profile.join("Cache"));
        let _ = std::fs::remove_dir_all(profile.join("Code Cache"));
    }
    let _ = std::fs::create_dir_all(&config_dir);
    let _ = std::fs::write(&marker, current);
}

fn save_notifications_in(dir: &Path, notifications: &[CustomNotification]) -> Result<(), String> {
    storage::save_json(&dir.join(NOTIFICATIONS_FILE), notifications)
}

/// Convertit un jour de la convention JS (0=dimanche..6=samedi, utilisee par l'ancien
/// reglage "Bilan de la semaine") vers la convention ISO (1=lundi..7=dimanche, utilisee
/// par `CustomNotification` et par Android).
fn migrate_legacy_day(js_day: u32) -> u32 {
    if js_day == 0 { 7 } else { js_day }
}

fn load_notifications(app: &tauri::AppHandle) -> Vec<CustomNotification> {
    match app.path().app_config_dir() {
        Ok(dir) => load_notifications_from(&dir),
        Err(_) => Vec::new(),
    }
}

/// Charge les notifications personnalisees depuis `dir`. Si aucun fichier n'existe mais que
/// l'ancien reglage "Bilan de la semaine" (mono-creneau) est present, le convertit
/// en une entree unique de la nouvelle liste puis la persiste, afin de ne perdre
/// aucune configuration existante de l'utilisateur lors de la mise a jour.
/// Un fichier existant mais illisible n'est pas migre : l'ancien reglage pourrait ressusciter
/// une notification que l'utilisateur avait supprimee depuis. Le fichier illisible est conserve
/// en `.corrupt` (voir `storage::load_json`).
fn load_notifications_from(dir: &Path) -> Vec<CustomNotification> {
    match storage::load_json::<Vec<CustomNotification>>(&dir.join(NOTIFICATIONS_FILE)) {
        LoadResult::Loaded(loaded) => return loaded,
        LoadResult::Corrupt => return Vec::new(),
        LoadResult::Missing => {}
    }

    let LoadResult::Loaded(cfg) = storage::load_json::<LegacyWeeklyNotifConfig>(&dir.join(LEGACY_NOTIFICATIONS_FILE))
    else {
        return Vec::new();
    };

    let iso_day = migrate_legacy_day(cfg.day_of_week);
    let migrated = vec![CustomNotification {
        id: "migrated-weekly-notif".to_string(),
        label: "Bilan de la semaine".to_string(),
        message: "C'est le moment de faire votre bilan hebdomadaire dans Ma Journée.".to_string(),
        time: format!("{:02}:{:02}", cfg.hour, cfg.minute),
        days: vec![iso_day],
        enabled: cfg.enabled,
        start_date: None,
        end_date: None,
    }];
    if let Err(e) = save_notifications_in(dir, &migrated) {
        log::warn!("Echec sauvegarde de la notification migree : {}", e);
    }
    migrated
}

/// Determine si `notif` doit se declencher a l'instant precis decrit par
/// `today`/`day`/`hour`/`minute`. Logique pure (aucun acces horloge/IO) pour rester
/// testable independamment du thread de la boucle de notification.
fn should_fire_now(notif: &CustomNotification, today: &str, day: u32, hour: u32, minute: u32) -> bool {
    if !notif.enabled || !notif.days.contains(&day) {
        return false;
    }
    if let Some(start) = &notif.start_date
        && today < start.as_str()
    {
        return false;
    }
    if let Some(end) = &notif.end_date
        && today > end.as_str()
    {
        return false;
    }
    notif.time == format!("{:02}:{:02}", hour, minute)
}

const MAX_LABEL_CHARS: usize = 200;
const MAX_MESSAGE_CHARS: usize = 2000;

/// `HH:MM` strict (deux chiffres partout) : `should_fire_now` compare des chaines, donc "8:00"
/// ne correspondrait jamais a "08:00".
fn is_valid_time(s: &str) -> bool {
    let Some((h, m)) = s.split_once(':') else { return false };
    let two_digits = |p: &str| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit());
    two_digits(h) && two_digits(m) && h.parse::<u32>().is_ok_and(|h| h < 24) && m.parse::<u32>().is_ok_and(|m| m < 60)
}

/// `YYYY-MM-DD` strict, date calendaire reelle (jours du mois et annees bissextiles). Le format
/// fixe est ce qui rend valide la comparaison lexicographique de `should_fire_now`.
fn is_valid_date(s: &str) -> bool {
    let mut parts = s.split('-');
    let (Some(y), Some(m), Some(d), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    if !(digits(y, 4) && digits(m, 2) && digits(d, 2)) {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (y.parse::<i32>(), m.parse::<u8>(), d.parse::<u8>()) else {
        return false;
    };
    time::Month::try_from(m).ok().and_then(|month| time::Date::from_calendar_date(y, month, d).ok()).is_some()
}

/// Verifie la liste recue du frontend (distant) avant de la persister. Le message d'erreur
/// designe l'entree (rang a partir de 1) et le champ, sans jamais recopier le contenu utilisateur.
fn validate_notifications(list: &[CustomNotification]) -> Result<(), String> {
    let mut ids = HashSet::new();
    for (i, n) in list.iter().enumerate() {
        let err = |field: &str, problem: &str| Err(format!("notification {} : {} {}", i + 1, field, problem));

        if n.id.is_empty() {
            return err("id", "vide");
        }
        if !ids.insert(n.id.as_str()) {
            return err("id", "en double");
        }
        if n.label.chars().count() > MAX_LABEL_CHARS {
            return err("label", "trop long");
        }
        if n.message.chars().count() > MAX_MESSAGE_CHARS {
            return err("message", "trop long");
        }
        if !is_valid_time(&n.time) {
            return err("time", "doit etre au format HH:MM");
        }
        let mut seen_days = [false; 8];
        for &day in &n.days {
            if !(1..=7).contains(&day) {
                return err("days", "doit contenir des jours de 1 (lundi) a 7 (dimanche)");
            }
            if std::mem::replace(&mut seen_days[day as usize], true) {
                return err("days", "contient un jour en double");
            }
        }
        for (field, date) in [("start_date", &n.start_date), ("end_date", &n.end_date)] {
            if date.as_deref().is_some_and(|d| !is_valid_date(d)) {
                return err(field, "doit etre une date au format YYYY-MM-DD");
            }
        }
        if let (Some(start), Some(end)) = (&n.start_date, &n.end_date)
            && start > end
        {
            return err("start_date", "posterieure a end_date");
        }
    }
    Ok(())
}

/// Au-dela, une notification manquee (mise en veille...) n'est plus rattrapee : un rappel
/// arrivant avec des heures de retard serait hors contexte.
const CATCH_UP_WINDOW: time::Duration = time::Duration::minutes(60);

fn truncate_to_minute(dt: time::PrimitiveDateTime) -> time::PrimitiveDateTime {
    dt.replace_second(0).and_then(|d| d.replace_nanosecond(0)).unwrap_or(dt)
}

/// Minutes locales a evaluer depuis le passage precedent `prev` (exclu) jusqu'a `now` (inclus).
/// `thread::sleep` etant fige pendant la mise en veille, l'ecart constate au reveil couvre les
/// minutes manquees, limitees a `CATCH_UP_WINDOW`. Premier passage ou horloge reculee : `now` seul.
fn minutes_to_check(
    prev: Option<time::PrimitiveDateTime>,
    now: time::PrimitiveDateTime,
) -> Vec<time::PrimitiveDateTime> {
    let now = truncate_to_minute(now);
    let Some(prev) = prev.map(truncate_to_minute) else { return vec![now] };
    if prev > now {
        return vec![now];
    }
    let mut minute = (prev + time::Duration::MINUTE).max(now - CATCH_UP_WINDOW + time::Duration::MINUTE);
    let mut minutes = Vec::new();
    while minute <= now {
        minutes.push(minute);
        minute += time::Duration::MINUTE;
    }
    minutes
}

/// Notifications a envoyer pour `minutes`. `last_fired` retient, par id, la derniere minute
/// (date comprise) de declenchement : une heure rejouee au passage a l'heure d'hiver ne
/// redeclenche pas, mais le meme creneau la semaine suivante si.
fn due_notifications<'a>(
    list: &'a [CustomNotification],
    minutes: &[time::PrimitiveDateTime],
    last_fired: &mut HashMap<String, time::PrimitiveDateTime>,
) -> Vec<&'a CustomNotification> {
    let mut due = Vec::new();
    for &minute in minutes {
        let today = format!("{:04}-{:02}-{:02}", minute.year(), u8::from(minute.month()), minute.day());
        let day = minute.weekday().number_from_monday() as u32;
        for notif in list {
            if should_fire_now(notif, &today, day, minute.hour() as u32, minute.minute() as u32)
                && last_fired.get(&notif.id) != Some(&minute)
            {
                last_fired.insert(notif.id.clone(), minute);
                due.push(notif);
            }
        }
    }
    // Purge les entrees dont l'id n'existe plus (notification supprimee entre-temps).
    last_fired.retain(|id, _| list.iter().any(|n| &n.id == id));
    due
}

fn start_notification_loop(notifications: SharedNotifications, app: tauri::AppHandle) {
    thread::spawn(move || {
        let mut last_fired = HashMap::new();
        let mut prev = None;
        loop {
            thread::sleep(Duration::from_secs(30));
            let list = notifications.lock().unwrap().clone();
            let now = time::OffsetDateTime::now_local().unwrap_or_else(|_| time::OffsetDateTime::now_utc());
            let now = time::PrimitiveDateTime::new(now.date(), now.time());
            for notif in due_notifications(&list, &minutes_to_check(prev, now), &mut last_fired) {
                send_notification(&app, &notif.label, &notif.message);
            }
            prev = Some(now);
        }
    });
}

/// Reaffiche et donne le focus a la fenetre principale (utilise par le tray, le
/// second lancement en single-instance, et le clic sur une notification). App verrouillee :
/// affiche l'ecran de verrouillage a la place.
fn show_and_focus_main_window(app: &tauri::AppHandle) {
    if lock::is_locked(app) {
        lock::show_window(app, lock::Mode::Unlock);
        return;
    }
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Envoie une notification systeme. Contrairement a `tauri_plugin_notification`, qui
/// n'expose aucun moyen de savoir si l'utilisateur clique dessus, on passe ici par
/// `notify_rust` directement (deja utilise en interne par le plugin) afin de pouvoir
/// reafficher et mettre au premier plan la fenetre principale au clic, quelle que soit
/// la notification (mise a jour, mail UCA, changement de planning...).
fn send_notification(app: &tauri::AppHandle, title: &str, body: &str) {
    let (title, body) = lock::mask_notification(app, title, body);
    let mut notification = notify_rust::Notification::new();
    notification.summary(title);
    notification.body(body);
    notification.auto_icon();
    // Convention freedesktop : declarer explicitement l'action "default" pour que le
    // clic sur le corps soit remonte meme par les daemons qui ne l'invoquent que si
    // elle a ete annoncee (ex. KDE Plasma). Ignore sur Windows (pas de bouton d'action
    // possible via ce backend), mais un simple clic y declenche quand meme l'evenement.
    notification.action("default", "default");

    #[cfg(windows)]
    {
        // Reprend la logique de tauri-plugin-notification (desktop.rs) : ne fixer
        // l'AppUserModelID que pour le binaire installe, pas en dev (sinon l'icone
        // resolue est celle du terminal/cargo plutot que celle de l'app).
        if let Ok(exe) = tauri::utils::platform::current_exe()
            && let Some(exe_dir) = exe.parent()
        {
            let dir = exe_dir.display().to_string();
            let sep = std::path::MAIN_SEPARATOR;
            let is_dev_target_dir =
                dir.ends_with(&format!("{sep}target{sep}debug")) || dir.ends_with(&format!("{sep}target{sep}release"));
            if !is_dev_target_dir {
                notification.app_id(&app.config().identifier);
            }
        }
    }

    match notification.show() {
        Ok(handle) => {
            let app = app.clone();
            thread::spawn(move || {
                handle.wait_for_action(|action| {
                    if action != "__closed" {
                        show_and_focus_main_window(&app);
                    }
                });
            });
        }
        // Titre non journalise : c'est du contenu utilisateur (notifications personnalisees).
        Err(e) => log::error!("Echec envoi notification : {}", e),
    }
}

#[tauri::command]
fn notify(app: tauri::AppHandle, title: String, body: String) {
    send_notification(&app, &title, &body);
}

#[tauri::command]
fn open_log_folder(app: tauri::AppHandle) -> Result<(), String> {
    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    // `opener` signale un chemin inexistant sous Linux : on s'assure donc que le dossier existe.
    let _ = std::fs::create_dir_all(&dir);
    open_in_browser(&dir.to_string_lossy())
}

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const UPDATE_RETRY_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// Apres un echec (ex: reseau indisponible juste apres le demarrage), nouvel essai rapide
/// plutot que d'attendre l'intervalle normal de 6 h.
fn next_update_check_delay(succeeded: bool) -> Duration {
    if succeeded { UPDATE_CHECK_INTERVAL } else { UPDATE_RETRY_INTERVAL }
}

async fn check_for_update(
    app: &tauri::AppHandle,
    pending_update: &PendingUpdateVersion,
    app_settings: &SharedAppSettings,
) -> Result<Option<String>, String> {
    use tauri_plugin_updater::UpdaterExt;
    let already_known = pending_update.lock().unwrap().clone();
    let updater = app.updater().map_err(|e| {
        log::error!("Updater indisponible: {}", e);
        e.to_string()
    })?;
    match updater.check().await {
        Ok(Some(update)) => {
            let version = update.version.clone();
            log::info!("Mise a jour disponible: v{}", version);
            *pending_update.lock().unwrap() = Some(version.clone());
            // N'avertit qu'une seule fois par version detectee, meme si le check tourne en boucle
            if already_known.as_deref() != Some(version.as_str()) && app_settings.lock().unwrap().update_notif_enabled {
                send_notification(
                    app,
                    "Mise à jour disponible",
                    &format!("Version {} disponible. Ouvrez Ma Journée pour l'installer.", version),
                );
            }
            Ok(Some(version))
        }
        Ok(None) => {
            log::info!("Aucune mise a jour disponible");
            Ok(None)
        }
        Err(e) => {
            log::error!("Echec verification mise a jour: {}", e);
            Err(e.to_string())
        }
    }
}

/// Verification manuelle declenchee depuis les reglages (bouton "Verifier maintenant").
/// Utile quand la verification automatique en arriere-plan n'a pas encore tourne ou a
/// echoue silencieusement (ex: reseau indisponible juste apres le lancement), laissant
/// l'etat "A jour" affiche par defaut sans avoir jamais reellement verifie.
#[tauri::command]
async fn check_for_update_now(
    app: tauri::AppHandle,
    pending_update: tauri::State<'_, PendingUpdateVersion>,
    app_settings: tauri::State<'_, SharedAppSettings>,
) -> Result<Option<String>, String> {
    let pending_update = pending_update.inner().clone();
    let app_settings = app_settings.inner().clone();
    check_for_update(&app, &pending_update, &app_settings).await
}

fn start_update_check_loop(
    app: tauri::AppHandle,
    pending_update: PendingUpdateVersion,
    app_settings: SharedAppSettings,
) {
    thread::spawn(move || {
        loop {
            let result = tauri::async_runtime::block_on(check_for_update(&app, &pending_update, &app_settings));
            thread::sleep(next_update_check_delay(result.is_ok()));
        }
    });
}

/// Sous GNOME (et derives : Ubuntu, Pop!_OS...), le module GTK `appmenu-gtk-module` est
/// charge par defaut via `GTK_MODULES` et tente de deporter la barre de menu vers le HUD
/// global de GNOME Shell, retire des versions recentes de GNOME. Resultat : le menu reste
/// pleinement fonctionnel (evenements, accelerateurs clavier) mais ne s'affiche nulle part,
/// y compris la barre classique sous le titre. On l'exclut explicitement pour forcer ce
/// rendu classique, sans toucher aux autres modules GTK eventuellement charges (ex: ibus).
#[cfg(target_os = "linux")]
fn disable_gtk_global_appmenu() {
    if let Ok(modules) = std::env::var("GTK_MODULES") {
        let filtered: Vec<&str> = modules.split(':').filter(|m| !m.contains("appmenu")).collect();
        // `set_var` est unsafe depuis l'edition 2024 (non thread-safe en general), mais sans
        // risque ici : appele en tout premier dans `run()`, avant que quoi que ce soit
        // (Tauri, GTK...) n'ait demarre le moindre thread susceptible de lire l'environnement.
        unsafe {
            std::env::set_var("GTK_MODULES", filtered.join(":"));
        }
    }
}

/// Site charge dans la fenetre principale (hote, URL). En debug (cargo run / tauri dev), le
/// serveur local demarre par beforeDevCommand (tauri.conf.json) plutot que la prod : sans ce
/// branchement, la fenetre chargeait toujours https://majournee.com meme en dev, rendant
/// devUrl/beforeDevCommand inoperants malgre une config par ailleurs correcte.
#[cfg(debug_assertions)]
const FRONTEND: (&str, &str) = ("localhost", "http://localhost:1420");
#[cfg(not(debug_assertions))]
const FRONTEND: (&str, &str) = ("majournee.com", "https://majournee.com");

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    disable_gtk_global_appmenu();

    let notifications: SharedNotifications = Arc::new(Mutex::new(Vec::new()));
    let pending_update: PendingUpdateVersion = Arc::new(Mutex::new(None));
    let app_settings: SharedAppSettings = Arc::new(Mutex::new(DesktopAppSettings::default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Deuxieme lancement : reafficher la fenetre existante au lieu de creer un
            // nouveau processus (qui dupliquerait l'icone tray).
            log::info!("Deuxieme lancement detecte, fenetre existante reaffichee");
            show_and_focus_main_window(app);
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .target(tauri_plugin_log::Target::new(
                    tauri_plugin_log::TargetKind::LogDir { file_name: Some("majournee-desktop".into()) },
                ))
                .level(log::LevelFilter::Info)
                .max_file_size(2_000_000)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepOne)
                .build(),
        )
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        // Taille/position/etat maximise memorises, restaures automatiquement a la creation de la
        // fenetre. VISIBLE est volontairement exclu : sinon le plugin reaffichait la fenetre au
        // demarrage --hidden si l'app avait ete quittee fenetre ouverte (Ctrl+Q). La visibilite
        // reste geree par --hidden et par le masquage dans le tray a la fermeture.
        .plugin(
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::SIZE
                        | tauri_plugin_window_state::StateFlags::POSITION
                        | tauri_plugin_window_state::StateFlags::MAXIMIZED,
                )
                .build(),
        )
        // Page locale de l'ecran de verrouillage (fonctionne hors ligne, voir lock.rs)
        .register_uri_scheme_protocol(lock::PROTOCOL, |_ctx, request| lock::serve_page(&request))
        .manage(notifications.clone())
        .manage(pending_update.clone())
        .manage(app_settings.clone())
        .setup(move |app| {
            // En tout premier : reverse un eventuel crash de la session precedente dans
            // le journal, puis installe le hook qui capturera un panic de celle-ci.
            report_previous_crash_if_any(app.handle());
            install_panic_hook(app.handle());

            // Charger les notifications personnalisees persistees (avec migration
            // automatique depuis l'ancien reglage "Bilan de la semaine" si besoin)
            let saved = load_notifications(app.handle());
            *notifications.lock().unwrap() = saved;

            let saved_settings = load_app_settings(app.handle());
            *app_settings.lock().unwrap() = saved_settings;

            // Verrouillage : l'app demarre verrouillee si le verrou est actif
            app.manage(lock::load(app.handle()));

            // Vide le cache WebKit si la version du binaire a change
            clear_webview_cache_if_updated(app.handle());

            // Lancement via l'autostart : demarre minimise dans le tray
            let start_hidden = std::env::args().any(|a| a == "--hidden");

            // Creation de la fenetre avec interception de navigation.
            let (frontend_host, frontend_url) = FRONTEND;

            // Barre de menu native (Fichier / Affichage). Les accelerateurs utilisent tous
            // au moins un modificateur (Ctrl/Cmd) : contrairement aux raccourcis lettre-seule
            // de shortcuts.js (cote web), un accelerateur de menu natif est intercepte au
            // niveau fenetre/OS quel que soit le focus DOM, y compris dans un champ de texte.
            // Un accelerateur sans modificateur interromprait donc la frappe normale.
            let menu = {
                let new_task     = MenuItem::with_id(app, "newTask", "Nouvelle tâche", true, Some("CmdOrCtrl+N"))?;
                let new_note     = MenuItem::with_id(app, "newNote", "Nouvelle note", true, Some("CmdOrCtrl+Shift+N"))?;
                let new_deadline = MenuItem::with_id(app, "newDeadline", "Nouvelle échéance", true, Some("CmdOrCtrl+D"))?;
                let lock_settings = MenuItem::with_id(app, "menu-lock", "Verrouillage…", true, None::<&str>)?;
                let quit         = MenuItem::with_id(app, "menu-quit", "Quitter", true, Some("CmdOrCtrl+Q"))?;
                let file_menu = Submenu::with_items(app, "Fichier", true, &[
                    &new_task, &new_note, &new_deadline,
                    &PredefinedMenuItem::separator(app)?,
                    &lock_settings,
                    &PredefinedMenuItem::separator(app)?,
                    &quit,
                ])?;

                let search      = MenuItem::with_id(app, "search", "Rechercher", true, Some("CmdOrCtrl+F"))?;
                let today       = MenuItem::with_id(app, "today", "Aujourd'hui", true, Some("CmdOrCtrl+Home"))?;
                let weekly      = MenuItem::with_id(app, "weekly", "Ma semaine", true, None::<&str>)?;
                let weekly_prep = MenuItem::with_id(app, "weeklyPrep", "Préparation de la semaine", true, None::<&str>)?;
                let refresh     = MenuItem::with_id(app, "refresh", "Actualiser", true, Some("CmdOrCtrl+R"))?;
                let settings    = MenuItem::with_id(app, "settings", "Réglages", true, Some("CmdOrCtrl+,"))?;
                let help        = MenuItem::with_id(app, "help", "Afficher les raccourcis", true, Some("F1"))?;
                let view_menu = Submenu::with_items(app, "Affichage", true, &[
                    &search, &today, &weekly, &weekly_prep, &refresh,
                    &PredefinedMenuItem::separator(app)?,
                    &settings, &help,
                ])?;

                Menu::with_items(app, &[&file_menu, &view_menu])?
            };

            // "menu-quit" et "menu-lock" sont traites directement ici (sans aller-retour JS) ;
            // tout autre id est relaye au frontend qui reutilise les memes handlers que les
            // raccourcis clavier web (buildShortcutHandlers()).
            app.on_menu_event(|app, event| {
                let id = event.id.as_ref();
                if id == "menu-quit" {
                    log::info!("Fermeture demandee via le menu Fichier");
                    app.exit(0);
                    return;
                }
                if id == "menu-lock" {
                    lock::open_settings(app);
                    return;
                }
                let _ = app.emit("native-menu", id.to_string());
            });

            let win = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(frontend_url.parse().unwrap()),
            )
            .title(if cfg!(debug_assertions) { "Ma Journée (DEV)" } else { "Ma Journée" })
            .menu(menu)
            .inner_size(1100.0, 750.0)
            .min_inner_size(380.0, 500.0)
            // Cachee a la creation : evite un flash a la taille par defaut avant que le plugin
            // window-state ne reapplique la taille/position memorisee.
            .visible(false)
            .initialization_script(r#"(function(){
                function openExt(url){if(window.__TAURI__)window.__TAURI__.core.invoke('open_url',{url:String(url)});}
                var _wo=window.open;
                window.open=function(url){if(url){openExt(url);return null;}return _wo.apply(this,arguments);};
                document.addEventListener('click',function(e){
                    var a=e.target.closest('a');
                    if(!a)return;
                    if(a.target==='_blank'||a.protocol==='mailto:'){e.preventDefault();openExt(a.href);}
                },true);
            })();"#)
            .on_navigation(move |url| match navigation_action(url, frontend_host) {
                NavigationAction::Stay => true,
                NavigationAction::OpenExternally => {
                    let url_str = url.to_string();
                    std::thread::spawn(move || { let _ = open_in_browser(&url_str); });
                    false
                }
                NavigationAction::Block => {
                    log::warn!("Navigation refusee : schema non autorise");
                    false
                }
            })
            .build()?;

            if !start_hidden {
                show_and_focus_main_window(app.handle());
            }

            // Linux : le relay mcp.majournee.com est desormais un sous-domaine de majournee.com
            // (charge dans la fenetre), donc le cookie de session mj_session est same-site.
            // WebKitGTK bloque par defaut uniquement les cookies TIERS
            // (WEBKIT_COOKIE_POLICY_ACCEPT_NO_THIRD_PARTY) ; un cookie same-site passe sans
            // contournement. Avant la migration mcp.jujuforge.fr -> mcp.majournee.com, ce cookie
            // etait cross-site et necessitait de forcer CookieAcceptPolicy::Always.

            // Fermer la fenetre -> minimiser dans le tray. La toute premiere fois,
            // une notification native explique que l'app reste active en arriere-plan
            // (sinon l'utilisateur pourrait croire qu'elle a quitte).
            let win_hide = win.clone();
            let close_app_handle = app.handle().clone();
            let close_app_settings = app_settings.clone();
            win.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                    api.prevent_close();
                    let _ = win_hide.hide();
                    log::info!("Fenetre fermee, application conservee en arriere-plan");

                    let mut settings = close_app_settings.lock().unwrap();
                    if !settings.tray_hint_shown {
                        settings.tray_hint_shown = true;
                        if let Err(e) = save_app_settings(&close_app_handle, &settings) {
                            log::warn!("Echec sauvegarde des reglages de l'app : {}", e);
                        }
                        // Sous Linux (libayatana), le clic sur l'icone ouvre le menu au lieu de la
                        // fenetre, et sans extension de tray (GNOME) l'icone n'apparait pas du tout :
                        // relancer l'app rouvre alors la fenetre (single-instance).
                        let body = if cfg!(target_os = "linux") {
                            "Fermer la fenêtre ne quitte pas l'application : elle reste active en arrière-plan. Pour la rouvrir, utilisez « Ouvrir Ma Journée » dans le menu de son icône, ou relancez l'application."
                        } else {
                            "Fermer la fenêtre ne quitte pas l'application : elle reste active dans la zone de notification, un clic sur son icône la rouvre."
                        };
                        send_notification(&close_app_handle, "Ma Journée continue en arrière-plan", body);
                    }
                    // Apres l'astuce ci-dessus, qui ne doit pas etre masquee.
                    lock::lock_now(&close_app_handle);
                }
            });

            // Icone tray avec menu
            // Item de statut non cliquable, mis a jour en direct depuis le web via
            // la commande `update_tray_status` (nombre de taches restantes, mails non lus...)
            let status_item =
                MenuItem::with_id(app, "status", "Ma Journée", false, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let show_item =
                MenuItem::with_id(app, "show", "Ouvrir Ma Journée", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&status_item, &separator, &show_item, &quit_item])?;
            app.manage(TrayStatusItem(status_item.clone()));

            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().ok_or("Icone par defaut introuvable pour le tray")?)
                .menu(&menu)
                .tooltip(if cfg!(debug_assertions) { "Ma Journée (DEV)" } else { "Ma Journée" })
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_and_focus_main_window(app),
                    "quit" => {
                        log::info!("Fermeture demandee via le menu de la zone de notification");
                        app.exit(0);
                    }
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    use tauri::tray::TrayIconEvent;
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: tauri::tray::MouseButton::Left,
                            ..
                        }
                    ) {
                        show_and_focus_main_window(tray.app_handle());
                    }
                })
                .build(app)?;
            log::info!("Icone tray creee");

            // Thread de declenchement des notifications personnalisees
            start_notification_loop(notifications.clone(), app.handle().clone());

            // Verification de mise a jour au demarrage, puis en boucle toutes les 6h
            // (l'app reste residente dans le tray, une session peut durer des jours sans redemarrage)
            start_update_check_loop(app.handle().clone(), pending_update.clone(), app_settings.clone());

            // Verrouillage au reveil apres une mise en veille
            lock::start_sleep_watchdog(app.handle().clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_custom_notifications, set_custom_notifications,
            get_pending_update, check_for_update_now, install_update, get_platform, get_app_version,
            get_autostart, set_autostart, open_url,
            get_update_notif_enabled, set_update_notif_enabled,
            notify, open_log_folder, update_tray_status,
            lock::lock_status, lock::lock_unlock_with_code, lock::lock_unlock_with_hello,
            lock::lock_configure, lock::lock_reset, lock::open_lock_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Ouvre une URL (ou un chemin de fichier/dossier) avec l'application par defaut, via l'API
/// native de chaque OS (`opener` -> ShellExecuteW sous Windows, xdg-open sous Linux). L'URL est
/// transmise telle quelle a l'API systeme, sans jamais passer par un shell : contrairement a
/// l'ancien `cmd /C start "" <url>`, aucun metacaractere (`&`, `"`, `^`...) n'est reinterprete,
/// ce qui ferme le risque d'injection de commande sous Windows. Le schema reste par ailleurs
/// filtre en amont par [is_allowed_url_scheme] pour les appels issus du frontend.
/// Retourne une erreur si aucune application n'a pu etre lancee (ex: aucun client mail
/// associe a mailto: sous Windows), pour que l'appelant JS (invoke) puisse le detecter
/// au lieu de supposer un succes silencieux (voir open_url et feedback.js).
fn open_in_browser(url: &str) -> Result<(), String> {
    opener::open(url).map_err(|e| {
        log::warn!("Echec ouverture externe: {}", e);
        e.to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_allowed_url_scheme() ─────────────────────────────────────────────

    #[test]
    fn allows_https_http_and_mailto() {
        assert!(is_allowed_url_scheme("https://majournee.com"));
        assert!(is_allowed_url_scheme("http://example.com"));
        assert!(is_allowed_url_scheme("mailto:foo@example.com"));
    }

    #[test]
    fn rejects_other_schemes_and_local_paths() {
        assert!(!is_allowed_url_scheme("file:///etc/passwd"));
        assert!(!is_allowed_url_scheme("javascript:alert(1)"));
        assert!(!is_allowed_url_scheme(r"\\host\evil.exe"));
        assert!(!is_allowed_url_scheme(""));
    }

    // ── navigation_action() ───────────────────────────────────────────────────

    fn action_for(url: &str) -> NavigationAction {
        navigation_action(&url.parse().unwrap(), "majournee.com")
    }

    #[test]
    fn stays_in_the_window_on_the_app_host() {
        assert!(matches!(action_for("https://majournee.com/taches?x=1"), NavigationAction::Stay));
    }

    #[test]
    fn opens_other_http_sites_and_mailto_externally() {
        assert!(matches!(action_for("https://example.com/"), NavigationAction::OpenExternally));
        assert!(matches!(action_for("http://example.com/"), NavigationAction::OpenExternally));
        assert!(matches!(action_for("mailto:a@example.com"), NavigationAction::OpenExternally));
        // Sous-domaine ou domaine voisin : ce n'est pas l'hote de l'app.
        assert!(matches!(action_for("https://evil-majournee.com/"), NavigationAction::OpenExternally));
    }

    #[test]
    fn blocks_other_schemes_instead_of_handing_them_to_the_os() {
        for url in ["file:///etc/passwd", "ftp://example.com/x", "data:text/html,hi", "javascript:alert(1)"] {
            assert!(matches!(action_for(url), NavigationAction::Block), "{url}");
        }
    }

    // ── has_version_changed() ────────────────────────────────────────────────

    #[test]
    fn no_change_when_versions_are_equal() {
        assert!(!has_version_changed("0.18.0", "0.18.0"));
    }

    #[test]
    fn change_detected_on_different_versions() {
        assert!(has_version_changed("0.17.0", "0.18.0"));
    }

    #[test]
    fn trailing_whitespace_in_last_is_ignored() {
        assert!(!has_version_changed("0.18.0\n", "0.18.0"));
    }

    #[test]
    fn empty_last_counts_as_a_change_first_run() {
        assert!(has_version_changed("", "0.18.0"));
    }

    // ── migrate_legacy_day() ─────────────────────────────────────────────────

    #[test]
    fn migrates_js_sunday_to_iso_seven() {
        assert_eq!(migrate_legacy_day(0), 7);
    }

    #[test]
    fn leaves_other_days_unchanged() {
        assert_eq!(migrate_legacy_day(1), 1);
        assert_eq!(migrate_legacy_day(6), 6);
    }

    // ── should_fire_now() ─────────────────────────────────────────────────────

    fn base_notif() -> CustomNotification {
        CustomNotification {
            id: "test".to_string(),
            label: "Titre".to_string(),
            message: "Message".to_string(),
            time: "08:00".to_string(),
            days: vec![1, 2, 3, 4, 5],
            enabled: true,
            start_date: None,
            end_date: None,
        }
    }

    #[test]
    fn fires_when_enabled_day_and_time_match() {
        let n = base_notif();
        assert!(should_fire_now(&n, "2026-08-24", 1, 8, 0));
    }

    #[test]
    fn does_not_fire_when_disabled() {
        let mut n = base_notif();
        n.enabled = false;
        assert!(!should_fire_now(&n, "2026-08-24", 1, 8, 0));
    }

    #[test]
    fn does_not_fire_on_a_day_not_selected() {
        let n = base_notif(); // lundi..vendredi
        assert!(!should_fire_now(&n, "2026-08-22", 6, 8, 0)); // samedi
    }

    #[test]
    fn does_not_fire_when_time_does_not_match() {
        let n = base_notif();
        assert!(!should_fire_now(&n, "2026-08-24", 1, 9, 0));
    }

    #[test]
    fn does_not_fire_before_start_date() {
        let mut n = base_notif();
        n.start_date = Some("2026-09-01".to_string());
        assert!(!should_fire_now(&n, "2026-08-24", 1, 8, 0));
    }

    #[test]
    fn does_not_fire_after_end_date() {
        let mut n = base_notif();
        n.end_date = Some("2026-08-01".to_string());
        assert!(!should_fire_now(&n, "2026-08-24", 1, 8, 0));
    }

    #[test]
    fn fires_on_start_and_end_date_boundaries_inclusive() {
        let mut n = base_notif();
        n.start_date = Some("2026-08-24".to_string());
        n.end_date = Some("2026-08-24".to_string());
        assert!(should_fire_now(&n, "2026-08-24", 1, 8, 0));
    }

    // ── JSON persiste (contrat avec le frontend et les versions precedentes) ───

    #[test]
    fn notification_json_uses_snake_case_keys() {
        let mut n = base_notif();
        n.start_date = Some("2026-09-01".to_string());
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json["start_date"], "2026-09-01");
        assert!(json["end_date"].is_null());
        assert_eq!(json["days"], serde_json::json!([1, 2, 3, 4, 5]));
    }

    #[test]
    fn notification_json_without_date_keys_loads_as_none() {
        let n: CustomNotification =
            serde_json::from_str(r#"{"id":"x","label":"L","message":"M","time":"08:00","days":[1],"enabled":true}"#)
                .unwrap();
        assert_eq!(n.start_date, None);
        assert_eq!(n.end_date, None);
    }

    #[test]
    fn app_settings_without_tray_hint_loads_with_default() {
        let s: DesktopAppSettings = serde_json::from_str(r#"{"update_notif_enabled":false}"#).unwrap();
        assert!(!s.update_notif_enabled);
        assert!(!s.tray_hint_shown);
    }

    // ── load_notifications_from() / save_notifications_in() ───────────────────

    #[test]
    fn notifications_survive_a_save_and_reload() {
        let tmp = storage::TempDir::new("notif-roundtrip");
        let mut n = base_notif();
        n.end_date = Some("2026-12-31".to_string());
        save_notifications_in(&tmp.0, std::slice::from_ref(&n)).unwrap();
        let loaded = load_notifications_from(&tmp.0);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, "test");
        assert_eq!(loaded[0].end_date.as_deref(), Some("2026-12-31"));
    }

    #[test]
    fn no_files_gives_an_empty_list() {
        let tmp = storage::TempDir::new("notif-empty");
        assert!(load_notifications_from(&tmp.0).is_empty());
    }

    #[test]
    fn legacy_weekly_setting_is_migrated_and_persisted() {
        let tmp = storage::TempDir::new("notif-legacy");
        std::fs::write(
            tmp.0.join(LEGACY_NOTIFICATIONS_FILE),
            r#"{"enabled":true,"day_of_week":0,"hour":7,"minute":5}"#,
        )
        .unwrap();
        let migrated = load_notifications_from(&tmp.0);
        assert_eq!(migrated.len(), 1);
        assert_eq!(migrated[0].days, vec![7]); // dimanche JS (0) -> ISO (7)
        assert_eq!(migrated[0].time, "07:05");
        assert!(migrated[0].enabled);
        // Persiste : un second chargement lit le nouveau fichier.
        assert!(tmp.0.join(NOTIFICATIONS_FILE).exists());
        assert_eq!(load_notifications_from(&tmp.0)[0].id, "migrated-weekly-notif");
    }

    #[test]
    fn corrupt_notifications_file_is_kept_and_not_replaced_by_the_legacy_setting() {
        let tmp = storage::TempDir::new("notif-corrupt");
        std::fs::write(tmp.0.join(NOTIFICATIONS_FILE), r#"[{"id":"a","la"#).unwrap();
        std::fs::write(
            tmp.0.join(LEGACY_NOTIFICATIONS_FILE),
            r#"{"enabled":true,"day_of_week":1,"hour":8,"minute":0}"#,
        )
        .unwrap();
        assert!(load_notifications_from(&tmp.0).is_empty());
        assert!(tmp.0.join("custom_notifications.json.corrupt").exists());
    }

    // ── validate_notifications() ──────────────────────────────────────────────

    fn invalid(mutate: impl FnOnce(&mut CustomNotification)) -> String {
        let mut n = base_notif();
        mutate(&mut n);
        validate_notifications(&[n]).unwrap_err()
    }

    #[test]
    fn accepts_a_valid_list_including_empty_days_and_dates() {
        let mut n = base_notif();
        n.days = vec![];
        n.start_date = Some("2024-02-29".to_string()); // annee bissextile
        n.end_date = Some("2024-02-29".to_string());
        let mut other = base_notif();
        other.id = "other".to_string();
        assert!(validate_notifications(&[n, other]).is_ok());
        assert!(validate_notifications(&[]).is_ok());
    }

    #[test]
    fn rejects_malformed_times() {
        for t in ["8:00", "24:00", "12:60", "08:5", "0800", "ab:cd", "", "08:00:00", "+8:00"] {
            let e = invalid(|n| n.time = t.to_string());
            assert!(e.contains("time"), "{t:?} aurait du etre refuse : {e}");
        }
    }

    #[test]
    fn accepts_boundary_times() {
        for t in ["00:00", "23:59"] {
            let mut n = base_notif();
            n.time = t.to_string();
            assert!(validate_notifications(&[n]).is_ok(), "{t}");
        }
    }

    #[test]
    fn rejects_days_out_of_range_or_duplicated() {
        assert!(invalid(|n| n.days = vec![0]).contains("days"));
        assert!(invalid(|n| n.days = vec![8]).contains("days"));
        assert!(invalid(|n| n.days = vec![1, 1]).contains("en double"));
    }

    #[test]
    fn rejects_malformed_or_impossible_dates() {
        for d in ["2026-13-01", "2026-1-1", "2026-02-30", "2025-02-29", "26-01-01", "2026/01/01", ""] {
            let e = invalid(|n| n.start_date = Some(d.to_string()));
            assert!(e.contains("start_date"), "{d:?} aurait du etre refuse : {e}");
            let e = invalid(|n| n.end_date = Some(d.to_string()));
            assert!(e.contains("end_date"), "{d:?} aurait du etre refuse : {e}");
        }
    }

    #[test]
    fn rejects_start_date_after_end_date() {
        let e = invalid(|n| {
            n.start_date = Some("2026-09-02".to_string());
            n.end_date = Some("2026-09-01".to_string());
        });
        assert!(e.contains("posterieure"));
    }

    #[test]
    fn rejects_empty_or_duplicated_ids() {
        assert!(invalid(|n| n.id = String::new()).contains("id"));
        let a = base_notif();
        let b = base_notif();
        assert!(validate_notifications(&[a, b]).unwrap_err().contains("en double"));
    }

    #[test]
    fn rejects_oversized_label_and_message() {
        assert!(invalid(|n| n.label = "a".repeat(MAX_LABEL_CHARS + 1)).contains("label"));
        assert!(invalid(|n| n.message = "a".repeat(MAX_MESSAGE_CHARS + 1)).contains("message"));
        let mut ok = base_notif();
        ok.label = "é".repeat(MAX_LABEL_CHARS); // compte en caracteres, pas en octets
        assert!(validate_notifications(&[ok]).is_ok());
    }

    #[test]
    fn error_names_the_entry_but_never_echoes_user_content() {
        let mut first = base_notif();
        first.id = "first".to_string();
        let mut second = base_notif();
        second.id = "second".to_string();
        second.label = "Secret perso".to_string();
        second.time = "99:99".to_string();
        let e = validate_notifications(&[first, second]).unwrap_err();
        assert!(e.contains("notification 2"));
        assert!(!e.contains("Secret") && !e.contains("99:99"));
    }

    // ── minutes_to_check() ────────────────────────────────────────────────────

    fn at(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> time::PrimitiveDateTime {
        time::Date::from_calendar_date(year, time::Month::try_from(month).unwrap(), day)
            .unwrap()
            .with_hms(hour, minute, second)
            .unwrap()
    }

    #[test]
    fn first_pass_checks_only_the_current_minute() {
        assert_eq!(minutes_to_check(None, at(2026, 8, 24, 8, 0, 42)), vec![at(2026, 8, 24, 8, 0, 0)]);
    }

    #[test]
    fn regular_30s_ticks_check_each_minute_once() {
        let a = at(2026, 8, 24, 8, 0, 10);
        let b = at(2026, 8, 24, 8, 0, 40);
        let c = at(2026, 8, 24, 8, 1, 10);
        assert!(minutes_to_check(Some(a), b).is_empty());
        assert_eq!(minutes_to_check(Some(b), c), vec![at(2026, 8, 24, 8, 1, 0)]);
    }

    #[test]
    fn wake_up_after_sleep_catches_up_missed_minutes() {
        let minutes = minutes_to_check(Some(at(2026, 8, 24, 7, 59, 50)), at(2026, 8, 24, 8, 40, 5));
        assert_eq!(minutes.len(), 41);
        assert_eq!(minutes[0], at(2026, 8, 24, 8, 0, 0));
        assert_eq!(*minutes.last().unwrap(), at(2026, 8, 24, 8, 40, 0));
    }

    #[test]
    fn catch_up_is_limited_to_the_window() {
        let minutes = minutes_to_check(Some(at(2026, 8, 24, 5, 0, 0)), at(2026, 8, 24, 8, 0, 0));
        assert_eq!(minutes.len(), 60);
        assert_eq!(minutes[0], at(2026, 8, 24, 7, 1, 0));
    }

    #[test]
    fn clock_going_back_checks_only_the_current_minute() {
        let minutes = minutes_to_check(Some(at(2026, 10, 25, 2, 59, 0)), at(2026, 10, 25, 2, 0, 20));
        assert_eq!(minutes, vec![at(2026, 10, 25, 2, 0, 0)]);
    }

    #[test]
    fn catch_up_crosses_midnight() {
        let minutes = minutes_to_check(Some(at(2026, 8, 23, 23, 58, 0)), at(2026, 8, 24, 0, 1, 0));
        assert_eq!(minutes, vec![at(2026, 8, 23, 23, 59, 0), at(2026, 8, 24, 0, 0, 0), at(2026, 8, 24, 0, 1, 0)]);
    }

    // ── due_notifications() ───────────────────────────────────────────────────

    #[test]
    fn single_day_notification_fires_again_the_following_week() {
        let mut n = base_notif();
        n.days = vec![1]; // lundi seulement
        let list = [n];
        let mut last_fired = HashMap::new();
        let monday = [at(2026, 8, 24, 8, 0, 0)];
        let next_monday = [at(2026, 8, 31, 8, 0, 0)];
        assert_eq!(due_notifications(&list, &monday, &mut last_fired).len(), 1);
        assert!(due_notifications(&list, &monday, &mut last_fired).is_empty());
        assert_eq!(due_notifications(&list, &next_monday, &mut last_fired).len(), 1);
    }

    #[test]
    fn missed_notification_is_sent_on_catch_up() {
        let list = [base_notif()]; // 08:00 en semaine
        let mut last_fired = HashMap::new();
        let minutes = minutes_to_check(Some(at(2026, 8, 24, 7, 30, 0)), at(2026, 8, 24, 8, 20, 0));
        assert_eq!(due_notifications(&list, &minutes, &mut last_fired).len(), 1);
    }

    #[test]
    fn deleted_notifications_are_purged_from_last_fired() {
        let mut last_fired = HashMap::from([("gone".to_string(), at(2026, 8, 24, 8, 0, 0))]);
        due_notifications(&[base_notif()], &[], &mut last_fired);
        assert!(last_fired.is_empty());
    }

    // ── progress_step() ───────────────────────────────────────────────────────

    #[test]
    fn progress_step_is_a_percentage_when_total_is_known() {
        assert_eq!(progress_step(0, Some(200)), 0);
        assert_eq!(progress_step(99, Some(200)), 49);
        assert_eq!(progress_step(200, Some(200)), 100);
        assert_eq!(progress_step(250, Some(200)), 100); // borne a 100 %
    }

    #[test]
    fn progress_step_counts_megabytes_when_total_is_unknown() {
        assert_eq!(progress_step(999_999, None), 0);
        assert_eq!(progress_step(2_500_000, None), 2);
        assert_eq!(progress_step(2_500_000, Some(0)), 2);
    }

    // ── next_update_check_delay() ─────────────────────────────────────────────

    #[test]
    fn update_check_retries_sooner_after_a_failure() {
        assert_eq!(next_update_check_delay(true), UPDATE_CHECK_INTERVAL);
        assert_eq!(next_update_check_delay(false), UPDATE_RETRY_INTERVAL);
        assert!(UPDATE_RETRY_INTERVAL < UPDATE_CHECK_INTERVAL);
    }
}
