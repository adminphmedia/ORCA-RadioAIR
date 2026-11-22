//! ORCA RadioAIR – desktopová aplikace (macOS, Windows). Spouští engine + lokální API
//! a v nativním okně zobrazuje UI (build z Lovable, případně vestavěný panel).

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

#[cfg(target_os = "macos")]
fn keep_awake() {
    // Vysílací software nesmí usnout: vypnout App Nap a zabránit uspání systému, dokud aplikace běží.
    let _ = std::process::Command::new("defaults").args(["write", "cz.orca.radioair", "NSAppSleepDisabled", "-bool", "YES"]).status();
    // caffeinate a prioritu nastavuje orca-core (general::keep_awake) podle Obecných nastavení
}
#[cfg(not(target_os = "macos"))]
fn keep_awake() {}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,symphonia=warn,symphonia_bundle_mp3=error")).init();
    keep_awake();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(tauri_plugin_autostart::MacosLauncher::LaunchAgent, None))
        .setup(|app| {
            // přibalené zdroje: ffmpeg a UI
            if let Ok(res) = app.path().resource_dir() {
                let ff = res.join("resources").join(if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" });
                if ff.is_file() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Ok(m) = std::fs::metadata(&ff) { if m.permissions().mode() & 0o111 == 0 { let _ = std::fs::set_permissions(&ff, std::fs::Permissions::from_mode(0o755)); } }
                    }
                    std::env::set_var("ORCA_FFMPEG", &ff);
                }
                let ui = res.join("ui");
                if ui.join("index.html").is_file() { std::env::set_var("ORCA_UI_DIR", &ui); }
            }
            let data_dir = std::env::var_os("ORCA_DATA_DIR").map(Into::into).unwrap_or_else(orca_core::default_data_dir);
            let orca = orca_core::Orca::start(&data_dir).map_err(|e| e.to_string())?;
            let bind = orca.cfg.read().api.bind.clone();
            app.manage(orca.clone());

            // spouštění se systémem – synchronizace s Obecnými nastaveními
            {
                use tauri_plugin_autostart::ManagerExt;
                let handle = app.handle().clone();
                let sync = move |want: bool| {
                    let al = handle.autolaunch();
                    let is = al.is_enabled().unwrap_or(false);
                    if want && !is { if let Err(e) = al.enable() { log::warn!("autostart: {e}"); } }
                    if !want && is { let _ = al.disable(); }
                };
                sync(orca.cfg.read().general.start_with_os);
                let (rx, cfg) = (orca.events.subscribe(), orca.cfg.clone());
                std::thread::Builder::new().name("orca-autostart".into()).spawn(move || {
                    while let Ok(ev) = rx.recv() { if matches!(ev, orca_core::events::Event::ConfigChanged) { sync(cfg.read().general.start_with_os); } }
                })?;
            }

            // API server ve vlastním tokio runtime
            let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
            {
                let orca = orca.clone();
                let bind = bind.clone();
                std::thread::Builder::new().name("orca-api".into()).spawn(move || {
                    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().worker_threads(2).build().expect("tokio");
                    rt.block_on(async move {
                        match tokio::net::TcpListener::bind(&bind).await {
                            Ok(l) => { let _ = tx.send(Ok(())); let _ = axum::serve(l, orca_server::router(orca)).await; }
                            Err(e) => { let _ = tx.send(Err(format!("Port {bind} je obsazený (běží už ORCA?): {e}"))); }
                        }
                    });
                })?;
            }
            rx.recv().map_err(|e| e.to_string())??;

            // samostatná okna panelů (více obrazovek) – pozice a velikost si pamatuje window-state
            {
                let handle = app.handle().clone();
                let bind = bind.clone();
                *orca.window_hook.write() = Some(Arc::new(move |panel: &str, title: &str| {
                    let label = format!("panel-{panel}");
                    if let Some(w) = handle.get_webview_window(&label) { let _ = w.unminimize(); let _ = w.show(); let _ = w.set_focus(); return Ok(()); }
                    let url = tauri::Url::parse(&format!("http://{bind}/?panel={panel}")).map_err(|e| e.to_string())?;
                    let (h2, title) = (handle.clone(), format!("ORCA RadioAIR – {title}"));
                    handle.run_on_main_thread(move || {
                        if let Err(e) = WebviewWindowBuilder::new(&h2, label, WebviewUrl::External(url)).disable_drag_drop_handler().title(title).inner_size(900.0, 700.0).min_inner_size(320.0, 240.0).build() {
                            log::error!("okno panelu: {e}");
                        }
                    }).map_err(|e| e.to_string())
                }));
            }

            let url = tauri::Url::parse(&format!("http://{bind}/")).map_err(|e| e.to_string())?;
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                // přetahování souborů (hudba, obrázky) z Finderu / Průzkumníka řeší webové rozhraní
                .disable_drag_drop_handler()
                .title("ORCA RadioAIR")
                .inner_size(1440.0, 900.0)
                .min_inner_size(1100.0, 700.0)
                .build()?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // okna panelů se zavírají normálně, potvrzení jen u hlavního okna
                if window.label() != "main" { return; }
                // zavření okna = konec vysílání → vyžádat potvrzení
                api.prevent_close();
                let w = window.clone();
                let lang = w.app_handle().state::<Arc<orca_core::Orca>>().cfg.read().station.language.clone();
                let (title, msg, yes, no) = if lang == "en" {
                    ("Quit ORCA RadioAIR?", "Broadcasting and all streams will stop.", "Quit", "Cancel")
                } else {
                    ("Ukončit ORCA RadioAIR?", "Vysílání i všechny streamy se zastaví.", "Ukončit", "Zrušit")
                };
                w.dialog().message(msg).title(title).kind(MessageDialogKind::Warning)
                    .buttons(MessageDialogButtons::OkCancelCustom(yes.into(), no.into()))
                    .show(move |ok| {
                        if ok {
                            let app = w.app_handle();
                            app.state::<Arc<orca_core::Orca>>().encoders.stop_all();
                            app.exit(0);
                        }
                    });
            }
        })
        .run(tauri::generate_context!())
        .expect("chyba při spuštění ORCA RadioAIR");
}
