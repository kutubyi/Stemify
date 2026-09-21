use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager};

const GONE_CHECKS: u32 = 3;
const CHECK_EVERY: Duration = Duration::from_secs(1);

pub struct Connected(pub AtomicBool);

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Waiting,
    Connected,
    Gone,
}

#[derive(Default)]
pub struct Watch {
    seen: bool,
    missing: u32,
}

impl Watch {
    pub fn observe(&mut self, running: bool) -> Verdict {
        if running {
            self.seen = true;
            self.missing = 0;
            Verdict::Connected
        } else if !self.seen {
            Verdict::Waiting
        } else {
            self.missing += 1;
            if self.missing >= GONE_CHECKS {
                *self = Watch::default(); // start over
                Verdict::Gone
            } else {
                Verdict::Connected
            }
        }
    }
}

pub fn start_watcher(app: AppHandle, on_gone: impl Fn(&AppHandle) + Send + 'static) {
    thread::spawn(move || {
        let mut watch = Watch::default();
        let mut last: Option<bool> = None;
        loop {
            thread::sleep(CHECK_EVERY);
            let Some(running) = crate::routing::spotify_running() else { continue };
            let verdict = watch.observe(running);
            if verdict == Verdict::Gone {
                on_gone(&app);
            }
            let connected = verdict == Verdict::Connected;
            if last != Some(connected) {
                last = Some(connected);
                app.state::<Connected>().0.store(connected, Ordering::Relaxed);
                let _ = app.emit("spotify-status", connected);
                eprintln!(
                    "[spotify] {}",
                    match verdict {
                        Verdict::Connected => "connected",
                        Verdict::Gone => "Spotify has quit: waiting for it to open again",
                        Verdict::Waiting => "waiting for Spotify to open",
                    }
                );
            }
        }
    });
}
