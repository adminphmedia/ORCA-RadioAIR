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
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
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

            // globální klávesové zkratky – fungují i když je ORCA v pozadí / minimalizovaná
            {
                use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
                let handle = app.handle().clone();
                let o2 = orca.clone();
                let apply = move || {
                    let (h2, o3) = (handle.clone(), o2.clone());
                    let _ = handle.run_on_main_thread(move || {
                        let gs = h2.global_shortcut();
                        let _ = gs.unregister_all();
                        let g = o3.cfg.read().general.clone();
                        if !g.global_hotkeys_enabled { return; }
                        for (action, combo) in g.global_hotkeys {
                            for k in combo.split(',').map(str::trim).filter(|k| !k.is_empty()) {
                                // bez modifikátoru by zkratka brala klávesu všem programům – ignorovat
                                if !k.contains('+') { log::warn!("globální zkratka {k} bez Ctrl/Alt/Cmd – přeskočena"); continue; }
                                match k.replace("Cmd", "Super").parse::<Shortcut>() {
                                    Ok(sc) => {
                                        let (o4, a) = (o3.clone(), action.clone());
                                        if let Err(e) = gs.on_shortcut(sc, move |_app, _sc, ev| { if ev.state() == ShortcutState::Pressed { o4.studio_action(&a); } }) {
                                            log::warn!("globální zkratku {k} nelze zaregistrovat (používá ji jiný program?): {e}");
                                            o3.events.alert("warning", "hotkeys", format!("Zkratku {k} nelze použít – nejspíš ji používá jiný program."));
                                        }
                                    }
                                    Err(e) => log::warn!("neplatná zkratka {k}: {e}"),
                                }
                            }
                        }
                    });
                };
                apply();
                let rx = orca.events.subscribe();
                std::thread::Builder::new().name("orca-hotkeys".into()).spawn(move || {
                    while let Ok(ev) = rx.recv() { if matches!(ev, orca_core::events::Event::ConfigChanged) { apply(); } }
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

            // aktualizace jedním tlačítkem (Obecné → Aktualizace); podpis balíčku ověřuje updater veřejným klíčem
            {
                use tauri_plugin_updater::UpdaterExt;
                // updater je async – volá se ve vlastním vlákně, aby nezáleželo na tom, odkud ho API zavolá
                fn run<T: Send + 'static>(f: impl std::future::Future<Output = Result<T, String>> + Send + 'static) -> Result<T, String> {
                    std::thread::spawn(move || tauri::async_runtime::block_on(f)).join().map_err(|_| "aktualizace: vnitřní chyba".to_string())?
                }
                let (h1, h2) = (app.handle().clone(), app.handle().clone());
                let orca2 = orca.clone();
                *orca.update_hook.write() = Some(orca_core::update::UpdateHook {
                    check: Arc::new(move || {
                        let h = h1.clone();
                        run(async move {
                            let u = h.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| format!("Kontrola aktualizací selhala: {e}"))?;
                            Ok(u.map(|u| orca_core::update::UpdateInfo { version: u.version.clone(), notes: u.body.clone().unwrap_or_default(), date: u.date.map(|d| d.to_string()).unwrap_or_default() }))
                        })
                    }),
                    install: Arc::new(move |state| {
                        let h = h2.clone();
                        let orca = orca2.clone();
                        run(async move {
                            let u = h.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?.ok_or("Nová verze už není k dispozici.")?;
                            let mut got: u64 = 0;
                            let st = state.clone();
                            let bytes = u.download(move |chunk, total| {
                                got += chunk as u64;
                                if let Some(t) = total { if t > 0 { st.lock().progress = Some(((got * 100) / t).min(100) as u8); } }
                            }, || {}).await.map_err(|e| format!("Stažení selhalo: {e}"))?;
                            state.lock().progress = Some(100);
                            // vysílání se na pár sekund přeruší: streamy zastavit čistě, pak instalovat a restartovat
                            orca.encoders.stop_all();
                            u.install(bytes).map_err(|e| format!("Instalace selhala: {e}"))?;
                            h.restart();
                        })
                    }),
                });
                // automatická kontrola: po spuštění a pak každých 6 hodin (jen upozorní, nic neinstaluje samo)
                let o = orca.clone();
                std::thread::Builder::new().name("orca-update-check".into()).spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(45));
                    let _ = o.update_check();
                    std::thread::sleep(std::time::Duration::from_secs(6 * 3600 - 45));
                })?;
            }

            // samostatná okna panelů (více obrazovek) – pozice a velikost si pamatuje window-state
            {
                let handle = app.handle().clone();
                let bind = bind.clone();
                *orca.window_hook.write() = Some(Arc::new(move |action: &str, panel: &str, title: &str, owner: &str| {
                    let label = format!("panel-{panel}");
                    if action == "close" {
                        if let Some(w) = handle.get_webview_window(&label) { let _ = w.close(); }
                        return Ok(());
                    }
                    if let Some(w) = handle.get_webview_window(&label) { let _ = w.unminimize(); let _ = w.show(); let _ = w.set_focus(); return Ok(()); }
                    let owner: String = owner.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
                    let url = tauri::Url::parse(&format!("http://{bind}/?panel={panel}&owner={owner}")).map_err(|e| e.to_string())?;
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
