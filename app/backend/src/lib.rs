mod routing;
mod spotify;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde::Serialize;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State};

#[derive(Clone, Serialize)]
struct EngineState {
    enabled: bool,
    semitones: i32,
    stems: Vec<String>,
    status: String,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            enabled: false,
            semitones: 0,
            stems: ["vocals", "drums", "bass", "guitar", "piano", "other"].map(String::from).to_vec(),
            status: "off".into(),
        }
    }
}

struct Engine(Arc<Mutex<EngineState>>);

fn publish(app: &AppHandle, state: &EngineState) {
    let _ = app.emit("engine-status", state.clone());
}

fn settle_later(app: AppHandle, state: Arc<Mutex<EngineState>>, ms: u64) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(ms));
        let mut s = state.lock().unwrap();
        if s.enabled && s.status == "loading" {
            s.status = "ready".into();
            publish(&app, &s);
        }
    });
}

fn update(app: &AppHandle, engine: &Engine, settle_ms: u64, change: impl FnOnce(&mut EngineState)) -> EngineState {
    let snapshot = {
        let mut s = engine.0.lock().unwrap();
        change(&mut s);
        s.status = if s.enabled { "loading" } else { "off" }.into();
        s.clone()
    };
    publish(app, &snapshot);
    if snapshot.enabled {
        settle_later(app.clone(), engine.0.clone(), settle_ms);
    }
    snapshot
}

#[tauri::command]
fn get_state(engine: State<Engine>) -> EngineState {
    engine.0.lock().unwrap().clone()
}

#[tauri::command]
fn spotify_connected(connected: State<spotify::Connected>) -> bool {
    connected.0.load(Ordering::Relaxed)
}

#[tauri::command]
fn set_enabled(enabled: bool, app: AppHandle, engine: State<Engine>) -> EngineState {
    update(&app, &engine, 2000, |s| s.enabled = enabled)
}

#[tauri::command]
fn set_pitch(semitones: i32, app: AppHandle, engine: State<Engine>) -> EngineState {
    update(&app, &engine, 800, |s| s.semitones = semitones.clamp(-12, 12))
}

#[tauri::command]
fn set_stems(stems: Vec<String>, app: AppHandle, engine: State<Engine>) -> EngineState {
    update(&app, &engine, 1500, |s| s.stems = stems)
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

fn restore_routing_on_exit() {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(routing::restore_spotify_if_on_cable());
    });
    match rx.recv_timeout(Duration::from_secs(3)) {
        Ok(Ok(message)) => eprintln!("[exit] routing: {message}"),
        Ok(Err(error)) => eprintln!("[exit] routing: could not check Spotify's output: {error}"),
        Err(_) => eprintln!("[exit] routing: timed out"),
    }
}

pub fn run() {
    tauri::Builder::default()
        .manage(Engine(Arc::new(Mutex::new(EngineState::default()))))
        .manage(spotify::Connected(AtomicBool::new(false)))
        .setup(|app| {
            setup_tray(app)?;
            spotify::start_watcher(app.handle().clone(), |app| {
                let engine = app.state::<Engine>();
                update(app, &engine, 0, |s| s.enabled = false);
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_state, spotify_connected, set_enabled, set_pitch, set_stems])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            if let tauri::RunEvent::Exit = event {
                restore_routing_on_exit();
            }
        });
}
