use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde::Serialize;

use crate::pipeline::Pipeline;
use crate::routing::{self, Restore, RouteError};

const TICK: Duration = Duration::from_millis(200);
/// Ticks in a row that must find Spotify missing before we believe it has quit (3 seconds).
const GONE_CHECKS: u32 = 15;
/// How often (in ticks) to look for a routing left behind (once a second).
const CHECK_EVERY_TICKS: u32 = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Off,
    /// Starting up.
    Loading,
    /// On, but Spotify has not played sound yet, so it cannot be routed.
    Waiting,
    /// On, but there is nothing to change yet, so Spotify plays untouched.
    Idle,
    /// Spotify's audio is flowing through Stemify.
    Ready,
}

#[derive(Clone, PartialEq, Debug, Serialize)]
pub struct State {
    pub enabled: bool,
    pub semitones: i32,
    pub stems: Vec<String>,
    pub status: Status,
    pub spotify_connected: bool,
    pub error: Option<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            enabled: false,
            semitones: 0,
            stems: ["vocals", "drums", "bass", "guitar", "piano", "other"].map(String::from).to_vec(),
            status: Status::Off,
            spotify_connected: false,
            error: None,
        }
    }
}

struct Inner {
    state: Mutex<State>,
    on_change: Box<dyn Fn(&State) + Send + Sync>,
    stop: AtomicBool,
}

impl Inner {
    fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    fn change(&self, apply: impl FnOnce(&mut State)) -> State {
        let (new, changed) = {
            let mut state = self.state.lock().unwrap();
            let old = state.clone();
            apply(&mut state);
            let changed = *state != old;
            (state.clone(), changed)
        };
        if changed {
            (self.on_change)(&new);
        }
        new
    }
}

pub struct Engine {
    inner: Arc<Inner>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Engine {
    pub fn start(on_change: impl Fn(&State) + Send + Sync + 'static) -> Engine {
        let inner = Arc::new(Inner { state: Mutex::new(State::default()), on_change: Box::new(on_change), stop: AtomicBool::new(false) });
        let supervisor = Supervisor { inner: inner.clone(), watch: Watch::default(), pipeline: None, routed: false, needs_check: true, ticks: 0 };
        let thread = thread::spawn(move || supervisor.run());
        Engine { inner, thread: Mutex::new(Some(thread)) }
    }

    pub fn state(&self) -> State {
        self.inner.state()
    }

    pub fn set_enabled(&self, enabled: bool) -> State {
        self.inner.change(|s| {
            if !enabled {
                s.enabled = false;
                s.status = Status::Off; // the loop finishes putting Spotify back
            } else if s.spotify_connected && !s.enabled {
                s.enabled = true;
                s.status = Status::Loading;
                s.error = None;
            }
        })
    }

    pub fn set_pitch(&self, semitones: i32) -> State {
        self.inner.change(|s| s.semitones = semitones.clamp(-12, 12))
    }

    pub fn set_stems(&self, stems: Vec<String>) -> State {
        self.inner.change(|s| s.stems = stems)
    }

    pub fn shutdown(&self) {
        self.inner.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.lock().unwrap().take() {
            let _ = thread.join();
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Waiting,
    Connected,
    Gone,
}

#[derive(Default)]
struct Watch {
    seen: bool,
    missing: u32,
}

impl Watch {
    fn observe(&mut self, running: bool) -> Verdict {
        if running {
            self.seen = true;
            self.missing = 0;
            Verdict::Connected
        } else if !self.seen {
            Verdict::Waiting
        } else {
            self.missing += 1;
            if self.missing >= GONE_CHECKS {
                *self = Watch::default();
                Verdict::Gone
            } else {
                Verdict::Connected
            }
        }
    }
}

struct Supervisor {
    inner: Arc<Inner>,
    watch: Watch,
    pipeline: Option<Pipeline>,
    /// Is Spotify currently routed to the cable by us?
    routed: bool,
    /// Might Spotify be left routed to the cable (a crash, or Spotify quitting while routed)?
    /// Checked whenever Spotify is running and we are not routing, until a check succeeds.
    needs_check: bool,
    ticks: u32,
}

impl Supervisor {
    fn run(mut self) {
        while !self.inner.stop.load(Ordering::Relaxed) {
            thread::sleep(TICK);
            self.ticks = self.ticks.wrapping_add(1);
            self.tick();
        }
        self.finish();
    }

    fn tick(&mut self) {
        // If we cannot tell (the process list failed), change nothing this round.
        let Some(running) = routing::spotify_running() else { return };
        let verdict = self.watch.observe(running);

        if verdict == Verdict::Gone {
            eprintln!("[spotify] Spotify has quit: waiting for it to open again");
            // Its routing can no longer be undone from here; it is reset when Spotify returns.
            self.pipeline = None;
            self.routed = false;
            self.needs_check = true;
            self.inner.change(|s| {
                s.enabled = false;
                s.status = Status::Off;
                s.spotify_connected = false;
                s.error = None;
            });
            return;
        }

        let connected = verdict == Verdict::Connected;
        if connected && !self.inner.state().spotify_connected {
            eprintln!("[spotify] connected");
            self.needs_check = true;
        }
        let state = self.inner.change(|s| s.spotify_connected = connected);

        if !state.enabled {
            self.stand_down(connected);
            self.inner.change(|s| s.status = Status::Off);
        } else if state.semitones == 0 {
            self.stand_down(connected);
            self.inner.change(|s| s.status = Status::Idle);
        } else {
            self.process(state.semitones);
        }
    }

    /// Something needs changing: make sure the pipeline is running and Spotify is routed to it.
    fn process(&mut self, semitones: i32) {
        match &self.pipeline {
            Some(pipeline) => pipeline.set_semitones(semitones),
            None => {
                self.inner.change(|s| s.status = Status::Loading);
                match Pipeline::start(semitones) {
                    Ok(pipeline) => self.pipeline = Some(pipeline),
                    Err(message) => return self.fail(message),
                }
            }
        }
        if !self.routed {
            match routing::route_spotify_to_cable() {
                Ok(_) => self.routed = true,
                Err(RouteError::NoAudioSession) => {
                    self.inner.change(|s| s.status = Status::Waiting);
                    return;
                }
                Err(RouteError::Failed(message)) => return self.fail(message),
            }
        }
        self.inner.change(|s| s.status = Status::Ready);
    }

    /// Nothing needs changing: leave Spotify untouched.
    fn stand_down(&mut self, spotify_connected: bool) {
        if self.routed {
            match routing::restore_spotify_if_on_cable() {
                Ok(result) => eprintln!("[routing] {result}"),
                Err(error) => eprintln!("[routing] could not reset Spotify's output: {error}"),
            }
            self.routed = false;
        }
        self.pipeline = None; // after the reset, so Spotify is never left without an output

        // A routing may have been left behind. It can only be read and reset once Spotify has
        // played sound, so keep asking until Spotify answers.
        if spotify_connected && self.needs_check && self.ticks % CHECK_EVERY_TICKS == 0 {
            match routing::restore_spotify_if_on_cable() {
                Ok(Restore::NoAudioSession) | Err(_) => {}
                Ok(result) => {
                    eprintln!("[routing] {result}");
                    self.needs_check = false;
                }
            }
        }
    }

    /// Turning on did not work: undo whatever was started and say why.
    fn fail(&mut self, message: String) {
        eprintln!("[engine] {message}");
        self.pipeline = None;
        self.inner.change(|s| {
            s.enabled = false;
            s.status = Status::Off;
            s.error = Some(message);
        });
    }

    /// The engine is shutting down: leave Spotify on its default output device.
    fn finish(&mut self) {
        match routing::restore_spotify_if_on_cable() {
            Ok(result) => eprintln!("[exit] routing: {result}"),
            Err(error) => eprintln!("[exit] routing: could not check Spotify's output: {error}"),
        }
        self.pipeline = None;
    }
}
