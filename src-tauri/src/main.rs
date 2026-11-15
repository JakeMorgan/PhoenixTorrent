#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod lazy_storage;

use std::{
    collections::HashSet,
    net::{Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};

use librqbit::{
    api::{ApiTorrentListOpts, TorrentIdOrHash},
    dht::DhtPersistenceConfig,
    limits::LimitsConfig,
    storage::StorageFactoryExt,
    AddTorrent, AddTorrentOptions, Api, DhtSessionConfig, ListenerMode, ListenerOptions, Session,
    SessionOptions, SessionPersistenceConfig,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, Manager, State, WindowEvent,
};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::OnceCell;

type R<T> = Result<T, String>;

fn e<E: std::fmt::Display>(err: E) -> String {
    format!("{err:#}")
}

fn to_json<T: Serialize>(v: T) -> R<Value> {
    serde_json::to_value(v).map_err(e)
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
struct Settings {
    download_dir: String,
    /// KiB/s, 0 = без ограничений
    down_limit_kbps: u32,
    up_limit_kbps: u32,
    port: u16,
    /// Крестик окна сворачивает в трей вместо выхода.
    close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let dir = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."));
        Self {
            download_dir: dir.to_string_lossy().into_owned(),
            down_limit_kbps: 0,
            up_limit_kbps: 0,
            port: 4240,
            close_to_tray: true,
        }
    }
}

fn bps(kbps: u32) -> Option<NonZeroU32> {
    NonZeroU32::new(kbps.saturating_mul(1024))
}

struct Core {
    api: Api,
    /// Папка, с которой создана сессия (меняется только после перезапуска).
    default_dir: PathBuf,
}

struct AppState {
    data_dir: PathBuf,
    settings: Mutex<Settings>,
    core: OnceCell<Core>,
    /// info hash -> папка раздачи, для хранилища с отложенным созданием файлов.
    folders: lazy_storage::Folders,
}

impl AppState {
    fn new(data_dir: PathBuf) -> Self {
        let settings = std::fs::read(data_dir.join("settings.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let folders: std::collections::HashMap<String, PathBuf> =
            std::fs::read(data_dir.join("folders.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
        Self {
            data_dir,
            settings: Mutex::new(settings),
            core: OnceCell::new(),
            folders: std::sync::Arc::new(std::sync::RwLock::new(folders)),
        }
    }

    fn save_folders(&self) {
        let map = self.folders.read().unwrap().clone();
        if let Ok(bytes) = serde_json::to_vec_pretty(&map) {
            let _ = std::fs::create_dir_all(&self.data_dir);
            let _ = std::fs::write(self.data_dir.join("folders.json"), bytes);
        }
    }

    fn settings(&self) -> Settings {
        self.settings.lock().unwrap().clone()
    }

    fn save_settings(&self) -> R<()> {
        let s = self.settings();
        std::fs::create_dir_all(&self.data_dir).map_err(e)?;
        let bytes = serde_json::to_vec_pretty(&s).map_err(e)?;
        std::fs::write(self.data_dir.join("settings.json"), bytes).map_err(e)
    }

    async fn core(&self) -> R<&Core> {
        self.core
            .get_or_try_init(|| self.init())
            .await
            .map_err(|err| format!("Не удалось запустить движок: {err:#}"))
    }

    async fn init(&self) -> anyhow::Result<Core> {
        let s = self.settings();
        let dir = PathBuf::from(&s.download_dir);
        std::fs::create_dir_all(&dir)?;
        std::fs::create_dir_all(&self.data_dir)?;

        let opts = |port: u16| SessionOptions {
            fastresume: true,
            default_storage_factory: Some(
                lazy_storage::LazyFsFactory {
                    folders: self.folders.clone(),
                }
                .boxed(),
            ),
            persistence: Some(SessionPersistenceConfig::Json {
                folder: Some(self.data_dir.join("session")),
            }),
            dht: Some(DhtSessionConfig {
                persistence: Some(DhtPersistenceConfig {
                    dump_interval: None,
                    config_filename: Some(self.data_dir.join("dht.json")),
                }),
                ..Default::default()
            }),
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpAndUtp,
                listen_addr: (Ipv6Addr::UNSPECIFIED, port).into(),
                enable_upnp_port_forwarding: true,
                ..Default::default()
            }),
            ratelimits: LimitsConfig {
                download_bps: bps(s.down_limit_kbps),
                upload_bps: bps(s.up_limit_kbps),
            },
            client_name_and_version: Some(concat!("Phoenix Torrent ", env!("CARGO_PKG_VERSION")).into()),
            ..Default::default()
        };

        // Если порт занят — пробуем случайный.
        let session = match Session::new_with_opts(dir.clone(), opts(s.port)).await {
            Ok(session) => session,
            Err(_) => Session::new_with_opts(dir.clone(), opts(0)).await?,
        };
        Ok(Core {
            api: Api::new(session, None),
            default_dir: dir,
        })
    }
}

#[tauri::command]
async fn snapshot(state: State<'_, AppState>) -> R<Value> {
    let core = state.core().await?;
    let list = core
        .api
        .api_torrent_list_ext(ApiTorrentListOpts { with_stats: true });
    Ok(json!({
        "torrents": to_json(list.torrents)?,
        "session": to_json(core.api.api_session_stats())?,
        "settings": state.settings(),
        "port": core.api.session().announce_port(),
    }))
}

/// Читает метаданные раздачи (из файла или по magnet-ссылке), ничего не скачивая.
#[tauri::command]
async fn preview(state: State<'_, AppState>, source: String) -> R<Value> {
    let core = state.core().await?;
    let add = AddTorrent::from_cli_argument(source.trim()).map_err(e)?;
    let opts = AddTorrentOptions {
        list_only: true,
        ..Default::default()
    };
    let fut = core.api.api_add_torrent(add, Some(opts));
    let resp = tokio::time::timeout(Duration::from_secs(120), fut)
        .await
        .map_err(|_| "Не удалось получить метаданные за 2 минуты — у раздачи нет доступных пиров".to_string())?
        .map_err(e)?;
    to_json(resp)
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
async fn add(
    state: State<'_, AppState>,
    source: String,
    only_files: Option<Vec<usize>>,
    folder: Option<String>,
    name: Option<String>,
    multi: bool,
    paused: bool,
    peers: Option<Vec<SocketAddr>>,
    info_hash: Option<String>,
    default_folder: Option<String>,
) -> R<Value> {
    let core = state.core().await?;
    let add = AddTorrent::from_cli_argument(source.trim()).map_err(e)?;

    // Для папки по умолчанию движок сам создаёт подпапку для многофайловых раздач;
    // для произвольной папки делаем то же самое вручную.
    let output_folder = folder
        .map(PathBuf::from)
        .filter(|f| *f != core.default_dir)
        .map(|f| match (&name, multi) {
            (Some(n), true) => f.join(sanitize(n)),
            _ => f,
        })
        .map(|f| f.to_string_lossy().into_owned())
        // Иначе — папка, которую движок сам предложил при предпросмотре.
        .or(default_folder);

    // Сообщаем хранилищу папку заранее: по ней оно создаёт файлы только при записи.
    if let (Some(hash), Some(dir)) = (&info_hash, &output_folder) {
        let known = core.api.session().get(TorrentIdOrHash::parse(hash).map_err(e)?);
        if known.is_none() {
            state
                .folders
                .write()
                .unwrap()
                .insert(hash.to_lowercase(), PathBuf::from(dir));
            state.save_folders();
        }
    }

    let opts = AddTorrentOptions {
        paused,
        only_files,
        output_folder,
        overwrite: true,
        initial_peers: peers,
        ..Default::default()
    };
    let resp = core.api.api_add_torrent(add, Some(opts)).await.map_err(e)?;
    to_json(resp)
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let s = s.trim().trim_end_matches('.').to_string();
    if s.is_empty() { "torrent".into() } else { s }
}

#[tauri::command]
async fn details(state: State<'_, AppState>, id: usize) -> R<Value> {
    let core = state.core().await?;
    to_json(
        core.api
            .api_torrent_details(TorrentIdOrHash::Id(id))
            .map_err(e)?,
    )
}

#[tauri::command]
async fn peers(state: State<'_, AppState>, id: usize) -> R<Value> {
    let core = state.core().await?;
    match core
        .api
        .api_peer_stats(TorrentIdOrHash::Id(id), Default::default())
    {
        Ok(p) => to_json(p),
        // Раздача на паузе — пиров нет.
        Err(_) => Ok(json!({ "peers": {} })),
    }
}

#[tauri::command]
async fn pause(state: State<'_, AppState>, id: usize) -> R<()> {
    let core = state.core().await?;
    core.api
        .api_torrent_action_pause(TorrentIdOrHash::Id(id))
        .await
        .map(|_| ())
        .map_err(e)
}

#[tauri::command]
async fn resume(state: State<'_, AppState>, id: usize) -> R<()> {
    let core = state.core().await?;
    core.api
        .api_torrent_action_start(TorrentIdOrHash::Id(id))
        .await
        .map(|_| ())
        .map_err(e)
}

#[tauri::command]
async fn remove(state: State<'_, AppState>, id: usize, delete_files: bool) -> R<()> {
    let core = state.core().await?;
    let id = TorrentIdOrHash::Id(id);
    if let Some(handle) = core.api.session().get(id) {
        let hash = handle.info_hash().as_string();
        state.folders.write().unwrap().remove(&hash);
        state.save_folders();
    }
    if delete_files {
        core.api.api_torrent_action_delete(id).await
    } else {
        core.api.api_torrent_action_forget(id).await
    }
    .map(|_| ())
    .map_err(e)
}

#[tauri::command]
async fn set_files(state: State<'_, AppState>, id: usize, files: Vec<usize>) -> R<()> {
    let core = state.core().await?;
    let set: HashSet<usize> = files.into_iter().collect();
    core.api
        .api_torrent_action_update_only_files(TorrentIdOrHash::Id(id), &set)
        .await
        .map(|_| ())
        .map_err(e)
}

#[tauri::command]
async fn set_limits(state: State<'_, AppState>, down: u32, up: u32) -> R<()> {
    let core = state.core().await?;
    let limits = &core.api.session().ratelimits;
    limits.set_download_bps(bps(down));
    limits.set_upload_bps(bps(up));
    {
        let mut s = state.settings.lock().unwrap();
        s.down_limit_kbps = down;
        s.up_limit_kbps = up;
    }
    state.save_settings()
}

#[tauri::command]
async fn set_download_dir(state: State<'_, AppState>, dir: String) -> R<()> {
    std::fs::create_dir_all(&dir).map_err(e)?;
    state.settings.lock().unwrap().download_dir = dir;
    state.save_settings()
}

#[tauri::command]
async fn pick_torrent_file(app: AppHandle) -> R<Option<String>> {
    Ok(app
        .dialog()
        .file()
        .add_filter("Торрент-файлы", &["torrent"])
        .blocking_pick_file()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

#[tauri::command]
async fn pick_folder(app: AppHandle) -> R<Option<String>> {
    Ok(app
        .dialog()
        .file()
        .blocking_pick_folder()
        .and_then(|p| p.into_path().ok())
        .map(|p| p.to_string_lossy().into_owned()))
}

#[tauri::command]
async fn open_folder(state: State<'_, AppState>, id: usize) -> R<()> {
    let core = state.core().await?;
    let d = core
        .api
        .api_torrent_details(TorrentIdOrHash::Id(id))
        .map_err(e)?;
    tauri_plugin_opener::open_path(d.output_folder, None::<&str>).map_err(e)
}

/// Путь к .torrent или magnet-ссылка из аргументов командной строки.
fn source_from_args<I: IntoIterator<Item = String>>(args: I) -> Option<String> {
    args.into_iter().skip(1).find(|a| !a.starts_with('-'))
}

#[tauri::command]
fn startup_arg() -> Option<String> {
    source_from_args(std::env::args())
}

#[tauri::command]
async fn set_close_to_tray(state: State<'_, AppState>, value: bool) -> R<()> {
    state.settings.lock().unwrap().close_to_tray = value;
    state.save_settings()
}

// ---------- Ассоциации с .torrent и magnet (только для текущего пользователя) ----------

const PROGID: &str = "PhoenixTorrent.torrent";

fn open_command() -> std::io::Result<(String, String)> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    Ok((format!("\"{exe}\" \"%1\""), format!("\"{exe}\",0")))
}

fn register_associations() -> std::io::Result<()> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let (cmd, icon) = open_command()?;
    let classes = RegKey::predef(HKEY_CURRENT_USER)
        .create_subkey("Software\\Classes")?
        .0;
    let set = |path: &str, name: &str, value: &str| -> std::io::Result<()> {
        classes.create_subkey(path)?.0.set_value(name, &value)
    };
    set(PROGID, "", "Торрент-файл")?;
    set(&format!("{PROGID}\\DefaultIcon"), "", &icon)?;
    set(&format!("{PROGID}\\shell\\open\\command"), "", &cmd)?;
    set(".torrent", "", PROGID)?;
    set(".torrent\\OpenWithProgids", PROGID, "")?;
    set("magnet", "", "URL:Magnet link")?;
    set("magnet", "URL Protocol", "")?;
    set("magnet\\DefaultIcon", "", &icon)?;
    set("magnet\\shell\\open\\command", "", &cmd)?;
    Ok(())
}

/// Команда открытия, записанная в реестре (если ассоциация уже регистрировалась).
fn registered_command() -> Option<String> {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(format!("Software\\Classes\\{PROGID}\\shell\\open\\command"))
        .ok()?
        .get_value("")
        .ok()
}

#[tauri::command]
fn associate() -> R<()> {
    register_associations().map_err(e)
}

#[tauri::command]
fn is_associated() -> bool {
    match (registered_command(), open_command()) {
        (Some(reg), Ok((cmd, _))) => reg == cmd,
        _ => false,
    }
}

fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn main() {
    // Регистрация ассоциаций без запуска окна: PhoenixTorrent.exe --register
    if std::env::args().any(|a| a == "--register") {
        if let Err(err) = register_associations() {
            eprintln!("{err}");
            std::process::exit(1);
        }
        return;
    }
    // Если программу переместили — обновляем путь в уже существующей ассоциации.
    if registered_command().is_some() && !is_associated() {
        let _ = register_associations();
    }

    tauri::Builder::default()
        // Повторный запуск (двойной щелчок по .torrent, magnet из браузера)
        // передаёт ссылку в уже работающее окно.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            show_main(app);
            if let Some(source) = source_from_args(argv) {
                let _ = app.emit("open-source", source);
            }
        }))
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let to_tray = window
                    .try_state::<AppState>()
                    .map(|s| s.settings().close_to_tray)
                    .unwrap_or(false);
                if to_tray {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app
                .path()
                .app_data_dir()
                .unwrap_or_else(|_| PathBuf::from("phoenix-data"));
            // Перенос данных из прежнего названия приложения (Tide).
            if let Some(old) = data_dir.parent().map(|p| p.join("app.tide.torrent")) {
                if !data_dir.exists() && old.exists() {
                    let _ = std::fs::rename(old, &data_dir);
                }
            }
            app.manage(AppState::new(data_dir));

            let show = MenuItem::with_id(app, "show", "Открыть Phoenix Torrent", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Выход", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("Phoenix Torrent")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_main(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            snapshot,
            preview,
            add,
            details,
            peers,
            pause,
            resume,
            remove,
            set_files,
            set_limits,
            set_download_dir,
            pick_torrent_file,
            pick_folder,
            open_folder,
            startup_arg,
            set_close_to_tray,
            associate,
            is_associated,
        ])
        .run(tauri::generate_context!())
        .expect("ошибка запуска Phoenix Torrent");
}
