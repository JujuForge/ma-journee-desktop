use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};
use tauri_plugin_window_state::WindowExt;

/// Notification programmee par l'utilisateur (titre, message, heure, jours de la
/// semaine, periode de validite optionnelle). Equivalent bureau de
/// `CustomNotification.kt` cote Android, pour une parite de fonctionnalite.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct CustomNotification {
    pub id: String,
    pub label: String,
    pub message: String,
    pub time: String,               // "HH:MM"
    pub days: Vec<u32>,             // 1=lundi..7=dimanche (convention ISO, identique a Android)
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
fn update_tray_status(state: tauri::State<'_, TrayStatusItem>, text: String) -> Result<(), String> {
    state.0.set_text(text).map_err(|e| e.to_string())
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
    settings.update_notif_enabled = enabled;
    save_app_settings(&app, &settings);
    Ok(())
}

#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app.updater().map_err(|e| e.to_string())?
        .check().await.map_err(|e| { log::error!("Echec check() pendant install_update: {}", e); e.to_string() })?
        .ok_or_else(|| "Aucune mise a jour disponible".to_string())?;
    log::info!("Installation de la mise a jour v{}", update.version);
    update.download_and_install(|_, _| {}, || {}).await
        .map_err(|e| { log::error!("Echec telechargement/installation: {}", e); e.to_string() })?;
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
    if let Err(e) = &result {
        log::error!("Echec configuration autostart (enabled={}): {}", enabled, e);
    }
    result.map_err(|e| e.to_string())
}

/// N'autorise que http/https/mailto. Sans ce garde, une chaine arbitraire (issue du
/// frontend distant) passee a `start`/`xdg-open` pourrait lancer un executable local
/// ou un chemin UNC (ex: `start "" \\host\evil.exe`).
fn is_allowed_url_scheme(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://") || url.starts_with("mailto:")
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
    { "linux" }
    #[cfg(target_os = "windows")]
    { "windows" }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    { "other" }
}

#[tauri::command]
fn get_custom_notifications(state: tauri::State<'_, SharedNotifications>) -> Vec<CustomNotification> {
    state.lock().unwrap().clone()
}

#[tauri::command]
fn set_custom_notifications(
    state: tauri::State<'_, SharedNotifications>,
    app: tauri::AppHandle,
    notifications: Vec<CustomNotification>,
) -> Result<(), String> {
    *state.lock().unwrap() = notifications.clone();
    save_notifications(&app, &notifications);
    Ok(())
}

fn legacy_config_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("weekly_notif.json"))
}

fn notifications_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("custom_notifications.json"))
}

fn app_settings_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("app_settings.json"))
}

fn save_app_settings(app: &tauri::AppHandle, settings: &DesktopAppSettings) {
    if let Some(path) = app_settings_path(app) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, serde_json::to_string(settings).unwrap_or_default()) {
            log::warn!("Echec sauvegarde des reglages de l'app ({}): {}", path.display(), e);
        }
    }
}

fn load_app_settings(app: &tauri::AppHandle) -> DesktopAppSettings {
    app_settings_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn crash_log_path(app: &tauri::AppHandle) -> std::path::PathBuf {
    app.path()
        .app_log_dir()
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("last_panic.log")
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
/// le message atteigne le disque avant la mort du processus.
fn install_panic_hook(app: &tauri::AppHandle) {
    let path = crash_log_path(app);
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
        // WebKitGTK (Linux)
        let _ = std::fs::remove_dir_all(data_dir.join("WebKitCache"));
        let _ = std::fs::remove_dir_all(data_dir.join("CacheStorage"));
        // WebView2 (Windows)
        let _ = std::fs::remove_dir_all(data_dir.join("EBWebView"));
    }
    let _ = std::fs::create_dir_all(&config_dir);
    let _ = std::fs::write(&marker, current);
}

fn save_notifications(app: &tauri::AppHandle, notifications: &[CustomNotification]) {
    if let Some(path) = notifications_path(app) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, serde_json::to_string(notifications).unwrap_or_default()) {
            log::warn!("Echec sauvegarde des notifications personnalisees ({}): {}", path.display(), e);
        }
    }
}

/// Convertit un jour de la convention JS (0=dimanche..6=samedi, utilisee par l'ancien
/// reglage "Bilan de la semaine") vers la convention ISO (1=lundi..7=dimanche, utilisee
/// par `CustomNotification` et par Android).
fn migrate_legacy_day(js_day: u32) -> u32 {
    if js_day == 0 { 7 } else { js_day }
}

/// Charge les notifications personnalisees. Si aucun fichier n'existe mais que
/// l'ancien reglage "Bilan de la semaine" (mono-creneau) est present, le convertit
/// en une entree unique de la nouvelle liste puis la persiste, afin de ne perdre
/// aucune configuration existante de l'utilisateur lors de la mise a jour.
fn load_notifications(app: &tauri::AppHandle) -> Vec<CustomNotification> {
    if let Some(path) = notifications_path(app) {
        if let Some(loaded) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<CustomNotification>>(&s).ok())
        {
            return loaded;
        }
    }

    let legacy = legacy_config_path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<LegacyWeeklyNotifConfig>(&s).ok());

    match legacy {
        Some(cfg) => {
            let iso_day = migrate_legacy_day(cfg.day_of_week);
            let migrated = vec![CustomNotification {
                id: "migrated-weekly-notif".to_string(),
                label: "Bilan de la semaine".to_string(),
                message: "C\'est le moment de faire votre bilan hebdomadaire dans Ma Journee.".to_string(),
                time: format!("{:02}:{:02}", cfg.hour, cfg.minute),
                days: vec![iso_day],
                enabled: cfg.enabled,
                start_date: None,
                end_date: None,
            }];
            save_notifications(app, &migrated);
            migrated
        }
        None => Vec::new(),
    }
}

/// Determine si `notif` doit se declencher a l'instant precis decrit par
/// `today`/`day`/`hour`/`minute`. Logique pure (aucun acces horloge/IO) pour rester
/// testable independamment du thread de la boucle de notification.
fn should_fire_now(notif: &CustomNotification, today: &str, day: u32, hour: u32, minute: u32) -> bool {
    if !notif.enabled || !notif.days.contains(&day) {
        return false;
    }
    if let Some(start) = &notif.start_date {
        if today < start.as_str() {
            return false;
        }
    }
    if let Some(end) = &notif.end_date {
        if today > end.as_str() {
            return false;
        }
    }
    notif.time == format!("{:02}:{:02}", hour, minute)
}

fn start_notification_loop(notifications: SharedNotifications, app: tauri::AppHandle) {
    thread::spawn(move || {
        let mut last_fired: HashMap<String, (u32, u32, u32)> = HashMap::new();
        loop {
            thread::sleep(Duration::from_secs(30));
            let list = notifications.lock().unwrap().clone();
            let now = time::OffsetDateTime::now_local()
                .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
            let day = now.weekday().number_from_monday() as u32;
            let hour = now.hour() as u32;
            let minute = now.minute() as u32;
            let today = format!("{:04}-{:02}-{:02}", now.year(), u8::from(now.month()), now.day());
            let key = (day, hour, minute);

            for notif in &list {
                if !should_fire_now(notif, &today, day, hour, minute) {
                    continue;
                }
                if last_fired.get(&notif.id) != Some(&key) {
                    last_fired.insert(notif.id.clone(), key);
                    send_notification(&app, &notif.label, &notif.message);
                }
            }
            // Purge les entrees dont l'id n'existe plus (notification supprimee entre-temps).
            last_fired.retain(|id, _| list.iter().any(|n| &n.id == id));
        }
    });
}

/// Reaffiche et donne le focus a la fenetre principale (utilise par le tray, le
/// second lancement en single-instance, et le clic sur une notification).
fn show_and_focus_main_window(app: &tauri::AppHandle) {
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
        if let Ok(exe) = tauri::utils::platform::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let dir = exe_dir.display().to_string();
                let sep = std::path::MAIN_SEPARATOR;
                let is_dev_target_dir = dir.ends_with(&format!("{sep}target{sep}debug"))
                    || dir.ends_with(&format!("{sep}target{sep}release"));
                if !is_dev_target_dir {
                    notification.app_id(&app.config().identifier);
                }
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
        Err(e) => log::error!("Echec envoi notification '{}': {}", title, e),
    }
}

#[tauri::command]
fn notify(app: tauri::AppHandle, title: String, body: String) {
    send_notification(&app, &title, &body);
}

#[tauri::command]
fn open_log_folder(app: tauri::AppHandle) -> Result<(), String> {
    let dir = app.path().app_log_dir().map_err(|e| e.to_string())?;
    let path_str = dir.to_string_lossy().to_string();
    #[cfg(target_os = "windows")]
    {
        if let Err(e) = std::process::Command::new("explorer").arg(&path_str).spawn() {
            log::warn!("Echec ouverture du dossier de logs (explorer): {}", e);
        }
    }
    #[cfg(not(target_os = "windows"))]
    { open_in_browser(&path_str); }
    Ok(())
}

const UPDATE_CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);

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
            if already_known.as_deref() != Some(version.as_str())
                && app_settings.lock().unwrap().update_notif_enabled
            {
                send_notification(
                    app,
                    "Mise a jour disponible",
                    &format!("Version {} disponible. Ouvrez Ma Journee pour l\'installer.", version),
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
            let _ = tauri::async_runtime::block_on(check_for_update(&app, &pending_update, &app_settings));
            thread::sleep(UPDATE_CHECK_INTERVAL);
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let notifications: SharedNotifications = Arc::new(Mutex::new(Vec::new()));
    let pending_update: PendingUpdateVersion = Arc::new(Mutex::new(None));
    let app_settings: SharedAppSettings = Arc::new(Mutex::new(DesktopAppSettings::default()));

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Deuxieme lancement : reafficher la fenetre existante au lieu de creer un
            // nouveau processus (qui dupliquerait l'icone tray).
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
        .plugin(tauri_plugin_window_state::Builder::default().build())
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

            // Vide le cache WebKit si la version du binaire a change
            clear_webview_cache_if_updated(app.handle());

            // Lancement via l'autostart : demarre minimise dans le tray
            let start_hidden = std::env::args().any(|a| a == "--hidden");

            // Creation de la fenetre avec interception de navigation.
            // En debug (cargo run / tauri dev), charge le serveur local demarre par
            // beforeDevCommand (tauri.conf.json) plutot que la prod : sans ce branchement,
            // la fenetre chargeait toujours https://majournee.com meme en dev, rendant
            // devUrl/beforeDevCommand inoperants malgre une config par ailleurs correcte.
            let (frontend_host, frontend_url) = if cfg!(debug_assertions) {
                ("localhost", "http://localhost:1420")
            } else {
                ("majournee.com", "https://majournee.com")
            };
            let win = tauri::WebviewWindowBuilder::new(
                app,
                "main",
                tauri::WebviewUrl::External(frontend_url.parse().unwrap()),
            )
            .title(if cfg!(debug_assertions) { "Ma Journée (DEV)" } else { "Ma Journée" })
            .inner_size(1100.0, 750.0)
            .min_inner_size(380.0, 500.0)
            // Cachee a la creation : evite un flash a la taille par defaut avant que
            // restore_state() ne reapplique la taille/position memorisee ci-dessous.
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
            .on_navigation(move |url| {
                if url.host_str() == Some(frontend_host) {
                    return true;
                }
                let url_str = url.to_string();
                std::thread::spawn(move || { let _ = open_in_browser(&url_str); });
                false
            })
            .build()?;

            // Restaure la taille/position/etat maximise memorises lors de la derniere
            // session. VISIBLE est volontairement exclu : la visibilite est deja geree
            // par --hidden et par le masquage dans le tray a la fermeture (cf. plus bas),
            // et laisser le plugin la restaurer entrerait en conflit avec cette logique.
            if let Err(e) = win.restore_state(
                tauri_plugin_window_state::StateFlags::SIZE
                    | tauri_plugin_window_state::StateFlags::POSITION
                    | tauri_plugin_window_state::StateFlags::MAXIMIZED,
            ) {
                log::warn!("Echec restauration de l'etat de la fenetre: {}", e);
            }
            if !start_hidden {
                let _ = win.show();
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

                    let mut settings = close_app_settings.lock().unwrap();
                    if !settings.tray_hint_shown {
                        settings.tray_hint_shown = true;
                        save_app_settings(&close_app_handle, &settings);
                        send_notification(
                            &close_app_handle,
                            "Ma Journée continue en arrière-plan",
                            "Fermer la fenêtre ne quitte pas l'application : elle reste active dans la zone de notification, un clic sur son icône la rouvre.",
                        );
                    }
                }
            });

            // Icone tray avec menu
            // Item de statut non cliquable, mis a jour en direct depuis le web via
            // la commande `update_tray_status` (nombre de taches restantes, mails non lus...)
            let status_item =
                MenuItem::with_id(app, "status", "Ma Journee", false, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let show_item =
                MenuItem::with_id(app, "show", "Ouvrir Ma Journee", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quitter", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&status_item, &separator, &show_item, &quit_item])?;
            app.manage(TrayStatusItem(status_item.clone()));

            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().ok_or("Icone par defaut introuvable pour le tray")?)
                .menu(&menu)
                .tooltip(if cfg!(debug_assertions) { "Ma Journee (DEV)" } else { "Ma Journee" })
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_and_focus_main_window(app),
                    "quit" => app.exit(0),
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

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_custom_notifications, set_custom_notifications,
            get_pending_update, check_for_update_now, install_update, get_platform, get_app_version,
            get_autostart, set_autostart, open_url,
            get_update_notif_enabled, set_update_notif_enabled,
            notify, open_log_folder, update_tray_status
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
}
