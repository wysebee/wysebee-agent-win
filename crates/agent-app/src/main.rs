//! Wysebee Agent for Windows's shell: an icon in the notification area (the system tray)
//! and one small window, over `agent-core`, which is the whole app bar these.
//!
//! - **No console window**: a release build is a Windows GUI program
//!   (`windows_subsystem = "windows"`); a debug build keeps its console, for the log.
//! - **The window** is `ui/` (plain HTML, CSS and JS, compiled in), shown by WebView2. It asks
//!   `status` each second and calls the commands below; closing it hides it.
//! - **The icon**: a click opens the window; its menu (right click) has Open Wysebee Agent,
//!   Pause / Resume, Quit.
//! - **A program or a skill that starts waiting for the person's Allow** brings the window
//!   forward (and flashes it in the taskbar).
//! - **Add a folder…** opens the native folder picker (tauri-plugin-dialog, called from
//!   here, so the page holds no dialog permission); the path picked goes to the core, which
//!   checks it.
//! - **Show in File Explorer** opens a skill's own folder with `explorer.exe`, the path as its
//!   one argument (never a shell).
//! - **The Server tab** saves the Wysebee Cloud address to pair with (the core's
//!   `set_cloud_url`: only while unpaired; it wins over `WYSEBEE_CLOUD_URL`, the default).
//! - What is real here and handed to the core: the state folder
//!   (`%LOCALAPPDATA%\Wysebee Agent`; `WYSEBEE_AGENT_STATE_DIR` moves it), the person's home
//!   (`%USERPROFILE%`), the `PATH` plus the app execution aliases' and Scoop's folders, the
//!   Windows Credential Manager and `WYSEBEE_CLOUD_URL`.
//!
//! It starts programs, so it runs with the person's own rights: no administrator, no service.

// A release build opens no console window beside the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("Wysebee Agent for Windows is a Windows app: build it on Windows.");

mod autostart;
mod screen;
mod tray;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use agent_core::secrets::{self, CredentialManager};
use agent_core::{agent, approvals, cloud, programs, Agent, Options, Timing};
use serde_json::Value;
use tauri::{AppHandle, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::DialogExt;

/// Moves the state folder (a second copy beside the installed one, for development).
const STATE_DIR_ENV: &str = "WYSEBEE_AGENT_STATE_DIR";

/// This PC's name (`COMPUTERNAME`, as Settings > System > About shows it); "PC" when there
/// is none.
fn device_name() -> String {
    std::env::var("COMPUTERNAME").ok().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "PC".into())
}

/// The folders added to the `PATH` programs are looked for in: those of app execution
/// aliases (`winget`, `python` from the Store), the Android SDK's `platform-tools` (adb) and
/// Scoop's shims, which a `PATH` read at sign-in may lack.
fn extra_program_folders(home: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    if let Some(local) = dirs::data_local_dir() {
        out.push(local.join("Microsoft").join("WindowsApps"));
        // adb, for the Android toolkit: where Android Studio puts the SDK.
        out.push(local.join("Android").join("Sdk").join("platform-tools"));
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(sdk) = std::env::var_os(var) {
            out.push(PathBuf::from(sdk).join("platform-tools"));
        }
    }
    out.push(home.join("scoop").join("shims"));
    out
}

/// Log lines go to the terminal when there is one, else to `agent.log` in the state
/// folder (truncated per launch). Never the secret, a blank's value or a program's output.
fn start_logging(state_dir: &Path) {
    let builder = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).with_ansi(false);
    if std::io::stderr().is_terminal() {
        builder.with_writer(std::io::stderr).init();
        return;
    }
    let _ = std::fs::create_dir_all(state_dir);
    match std::fs::File::create(state_dir.join("agent.log")) {
        Ok(file) => builder.with_writer(Mutex::new(file)).init(),
        // An unwritable folder costs the log, never the launch.
        Err(_) => builder.with_writer(std::io::sink).init(),
    }
}

/// Bring the window forward.
pub fn show_window(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// What the window shows: the core's status, and whether the app starts at sign-in.
#[tauri::command]
async fn status(agent: State<'_, Agent>) -> Result<Value, String> {
    // Once a launch: the page loaded and reached the shell.
    static ASKED: std::sync::Once = std::sync::Once::new();
    ASKED.call_once(|| tracing::info!("window_ready"));
    let mut status = serde_json::to_value(agent.status()).map_err(|e| e.to_string())?;
    status["autostart"] = Value::Bool(autostart::is_enabled());
    Ok(status)
}

/// The Wysebee Cloud server to pair with, from the Server tab ("" for the default). The
/// core checks it (https, or http to this computer) and refuses while paired.
#[tauri::command]
async fn set_cloud_url(agent: State<'_, Agent>, url: String) -> Result<(), String> {
    agent.set_cloud_url(&url)
}

#[tauri::command]
async fn start_pairing(agent: State<'_, Agent>, name: String) -> Result<(), String> {
    agent.start_pairing(&name).await
}

#[tauri::command]
async fn cancel_pairing(agent: State<'_, Agent>) -> Result<(), String> {
    agent.cancel_pairing().await;
    Ok(())
}

#[tauri::command]
async fn allow(agent: State<'_, Agent>, name: String) -> Result<(), String> {
    agent.allow(&name)
}

#[tauri::command]
async fn revoke(agent: State<'_, Agent>, name: String) -> Result<(), String> {
    agent.revoke(&name);
    Ok(())
}

#[tauri::command]
async fn allow_skill(agent: State<'_, Agent>, name: String) -> Result<(), String> {
    agent.allow_skill(&name).await
}

#[tauri::command]
async fn revoke_skill(agent: State<'_, Agent>, name: String) -> Result<(), String> {
    agent.revoke_skill(&name);
    Ok(())
}

/// Open a skill's folder on this PC in File Explorer, made first when it is not there: the
/// person's own files put there stay on the PC and are never sent. `explorer.exe` is started
/// with the path as its one argument; it answers 1 even when it opened the folder, so its
/// exit code says nothing.
#[tauri::command]
async fn show_skill_folder(agent: State<'_, Agent>, name: String) -> Result<(), String> {
    let folder = agent.skill_folder(&name)?;
    let windows = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    std::process::Command::new(windows.join("explorer.exe")).arg(&folder).spawn().map(|_| ()).map_err(|e| format!("File Explorer could not be asked: {e}"))
}

/// The native folder picker, over the window: the folder picked (none: cancelled).
#[tauri::command]
async fn choose_folder(app: AppHandle) -> Result<Option<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut picker = app.dialog().file().set_title("Choose a folder for Wysebee");
    if let Some(w) = app.get_webview_window("main") {
        picker = picker.set_parent(&w);
    }
    picker.pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    let picked = rx.await.map_err(|_| "The folder picker closed.".to_string())?;
    match picked {
        None => Ok(None),
        Some(p) => p.into_path().map(|p| Some(p.display().to_string())).map_err(|e| format!("That folder could not be used: {e}")),
    }
}

#[tauri::command]
async fn add_folder(agent: State<'_, Agent>, path: String, name: String, mode: String, description: String) -> Result<String, String> {
    agent.add_folder(&path, &name, &mode, &description)
}

#[tauri::command]
async fn update_folder(agent: State<'_, Agent>, id: String, name: String, mode: String, description: String) -> Result<(), String> {
    agent.update_folder(&id, &name, &mode, &description)
}

#[tauri::command]
async fn remove_folder(agent: State<'_, Agent>, id: String) -> Result<(), String> {
    agent.remove_folder(&id)
}

#[tauri::command]
async fn set_paused(agent: State<'_, Agent>, paused: bool) -> Result<(), String> {
    agent.set_paused(paused).await;
    Ok(())
}

#[tauri::command]
async fn unpair(agent: State<'_, Agent>) -> Result<(), String> {
    agent.unpair().await;
    Ok(())
}

/// "Let Wysebee run any program on this PC", on or off (the window asks first before on).
#[tauri::command]
async fn set_any_program(agent: State<'_, Agent>, on: bool) -> Result<(), String> {
    agent.set_any_program(on);
    Ok(())
}

/// "Let Wysebee use this PC as a developer", on or off (the window asks first before on).
#[tauri::command]
async fn set_dev_access(agent: State<'_, Agent>, on: bool) -> Result<(), String> {
    agent.set_dev_access(on);
    Ok(())
}

/// "Let Wysebee use Android phones on this PC", on or off (the window asks first before on).
#[tauri::command]
async fn set_android_dev(agent: State<'_, Agent>, on: bool) -> Result<(), String> {
    agent.set_android_dev(on);
    Ok(())
}

/// "Let Wysebee see and control this PC's screen", on or off (the window asks first before on).
#[tauri::command]
async fn set_screen_control(agent: State<'_, Agent>, on: bool) -> Result<(), String> {
    agent.set_screen_control(on);
    Ok(())
}

/// Start at sign-in, or not ([`autostart`]: the registry's Run key).
#[tauri::command]
async fn set_autostart(on: bool) -> Result<(), String> {
    autostart::set(on)
}

/// The window's own title bar follows the page's Appearance: "light", "dark", or none
/// (Windows' setting).
#[tauri::command]
fn set_theme(window: tauri::WebviewWindow, theme: Option<String>) -> Result<(), String> {
    let theme = match theme.as_deref() {
        Some("light") => Some(tauri::Theme::Light),
        Some("dark") => Some(tauri::Theme::Dark),
        _ => None,
    };
    window.set_theme(theme).map_err(|e| e.to_string())
}

fn main() {
    let home = dirs::home_dir().expect("this account has no home folder");
    let local = dirs::data_local_dir().unwrap_or_else(|| home.join("AppData").join("Local"));
    let default_dir = approvals::default_dir(&local);
    let state_dir = std::env::var_os(STATE_DIR_ENV).map(PathBuf::from).unwrap_or_else(|| default_dir.clone());
    // Two instances would take each other's calls: the second one ends at once — before
    // the log is opened, which would empty the first one's — and brings the first one's
    // window forward, so opening the app again is never met with nothing.
    let Some(_lock) = agent::single_instance(&state_dir) else {
        if !agent::wake_running(&state_dir) {
            eprintln!("Wysebee Agent is already running.");
        }
        return;
    };
    start_logging(&state_dir);

    // The screen for screen control: the ring's window is made once the app runs.
    let desktop = Arc::new(screen::Desktop::default());
    let agent = Agent::open(Options {
        state_dir: state_dir.clone(),
        home: home.clone(),
        path: Some(programs::search_path(std::env::var_os("PATH").as_deref(), &extra_program_folders(&home))),
        cloud_url: std::env::var(cloud::ENV).ok(),
        // The installed app's folder keeps `link-secret`; another folder has its own item.
        secrets: Arc::new(CredentialManager { account: secrets::credential_account(&state_dir, &default_dir) }),
        device_name: device_name(),
        version: agent_core::VERSION.into(),
        timing: Timing::default(),
        screen: Some(desktop.clone()),
    });
    tracing::info!(version = agent_core::VERSION, state_dir = %state_dir.display(), cloud = %agent.cloud().0, "agent_starting");

    let core = agent.clone();
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(agent)
        .invoke_handler(tauri::generate_handler![status, start_pairing, cancel_pairing, allow, revoke, allow_skill, revoke_skill, show_skill_folder, choose_folder, add_folder, update_folder, remove_folder, set_paused, set_any_program, set_screen_control, set_dev_access, set_android_dev, unpair, set_cloud_url, set_autostart, set_theme])
        .setup(move |app| {
            // Shown at launch only when there is something to do in it: pairing, or a
            // program or a skill waiting to be allowed. Otherwise the app starts in the notification area.
            let first = core.status();
            let show = first.state != "paired" || first.waiting > 0;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("Wysebee Agent")
                .inner_size(920.0, 700.0)
                .min_inner_size(600.0, 480.0)
                .center()
                .visible(show)
                // Only the app's own page loads here.
                .on_navigation(|url| matches!(url.scheme(), "tauri") || url.host_str() == Some("tauri.localhost"))
                .build()?;
            tray::install(app.handle(), core.clone())?;
            desktop.attach(app.handle())?;
            app.manage(desktop.clone());

            // A program or a skill started waiting for the person's Allow: bring the window forward.
            let handle = app.handle().clone();
            core.set_attention_hook(Some(Arc::new(move || {
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || {
                    show_window(&h);
                    if let Some(w) = h.get_webview_window("main") {
                        let _ = w.request_user_attention(Some(tauri::UserAttentionType::Informational));
                    }
                });
            })));

            // Opened again (the Start menu, a shortcut, the installer's last page): Windows
            // starts a second process, which knocks; the window shows.
            let handle = app.handle().clone();
            let woken = agent::listen_for_wake(&state_dir, move || {
                let h = handle.clone();
                let _ = handle.run_on_main_thread(move || show_window(&h));
            });
            if let Err(e) = woken {
                tracing::warn!(error = %e, "wake_listen_failed");
            }

            // The link's loop, for as long as the app runs.
            let looping = core.clone();
            tauri::async_runtime::spawn(async move { looping.run().await });
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window hides it; the app stays in the notification area. Quit is in its menu.
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("the app could not be built");

    app.run(|_, _| {});
}
