use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};

struct EngineHandle(Arc<engine::Engine>);

#[tauri::command]
fn get_state(handle: State<EngineHandle>) -> engine::State {
    handle.0.state()
}

#[tauri::command]
fn set_enabled(enabled: bool, handle: State<EngineHandle>) -> engine::State {
    handle.0.set_enabled(enabled)
}

#[tauri::command]
fn set_pitch(semitones: i32, handle: State<EngineHandle>) -> engine::State {
    handle.0.set_pitch(semitones)
}

#[tauri::command]
fn set_stems(stems: Vec<String>, handle: State<EngineHandle>) -> engine::State {
    handle.0.set_stems(stems)
}

#[tauri::command]
fn set_keep_model(keep: bool, handle: State<EngineHandle>) -> engine::State {
    handle.0.set_keep_model(keep)
}

fn show_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn setup_tray(app: &tauri::App) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show Stemify", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Stemify", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::new()
        .icon(app.default_window_icon().expect("app icon").clone())
        .tooltip("Stemify")
        .menu(&menu)
        .show_menu_on_left_click(false) // left click = show the window, right click = menu
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn shutdown_engine(engine: Arc<engine::Engine>) {
    let (done, wait) = mpsc::channel();
    thread::spawn(move || {
        engine.shutdown();
        let _ = done.send(());
    });
    if wait.recv_timeout(Duration::from_secs(3)).is_err() {
        eprintln!("[exit] the engine took too long to stop, so it was skipped");
    }
}

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let events = app.handle().clone();
            let engine = engine::Engine::start(move |state| {
                let _ = events.emit("engine-status", state.clone());
            });
            app.manage(EngineHandle(Arc::new(engine)));
            setup_tray(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_state, set_enabled, set_pitch, set_stems, set_keep_model])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                shutdown_engine(app.state::<EngineHandle>().0.clone());
            }
        });
}
