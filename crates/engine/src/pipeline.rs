use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::routing::{com_init, outputs};

type WinResult<T> = windows::core::Result<T>;

const RATE: usize = 44_100; 
const CH: usize = 2;
const PREBUFFER_MS: usize = 100; 
const MAX_DEPTH_MS: usize = 400; 
const TRIM_TO_MS: usize = 150;

fn fmt() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CH as u16,
        nSamplesPerSec: RATE as u32,
        nAvgBytesPerSec: (RATE * CH * 4) as u32,
        nBlockAlign: (CH * 4) as u16,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

const FLAGS_CONVERT: u32 = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

#[derive(Default)]
struct Shared {
    ring: Mutex<VecDeque<f32>>,
    stop: AtomicBool,
    underruns: AtomicU64,
}

pub struct Pipeline {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl Pipeline {
    pub fn start() -> Result<Pipeline, String> {
        com_init().map_err(|e| e.to_string())?;

        let cable = outputs()
            .map_err(|e| e.to_string())?
            .into_iter()
            .find(|(_, name)| name.to_lowercase().contains("cable input"))
            .ok_or("No virtual cable is installed.")?;
        let output_id = default_output_id().map_err(|e| e.to_string())?;
        if output_id == cable.0 {
            return Err("The default output device is the virtual cable. Choose your speakers or headphones as the Windows default output.".into());
        }

        let shared = Arc::new(Shared::default());
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let mut threads = Vec::new();
        {
            let (shared, tx, id) = (shared.clone(), tx.clone(), cable.0);
            threads.push(thread::spawn(move || capture_thread(&id, &shared, tx)));
        }
        {
            let shared = shared.clone();
            threads.push(thread::spawn(move || render_thread(&output_id, &shared, tx)));
        }

        let pipeline = Pipeline { shared, threads };
        for _ in 0..2 {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => {}
                Ok(Err(message)) => return Err(message),
                Err(_) => return Err("Timed out starting the audio.".into()),
            }
        }
        Ok(pipeline)
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        eprintln!("[audio] pipeline stopped ({} underruns)", self.shared.underruns.load(Ordering::Relaxed));
    }
}

fn enumerator() -> WinResult<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn default_output_id() -> WinResult<String> {
    unsafe { Ok(enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?.GetId()?.to_string()?) }
}

struct Capture {
    client: IAudioClient,
    capture: IAudioCaptureClient,
}

fn capture_init(id: &str) -> WinResult<Capture> {
    unsafe {
        let device = enumerator()?.GetDevice(&HSTRING::from(id))?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK | FLAGS_CONVERT, 2_000_000, 0, &fmt(), None)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok(Capture { client, capture })
    }
}

fn capture_thread(id: &str, shared: &Shared, ready: mpsc::Sender<Result<(), String>>) {
    if com_init().is_err() {
        let _ = ready.send(Err("Could not start the audio system.".into()));
        return;
    }
    let capture = match capture_init(id) {
        Ok(capture) => capture,
        Err(e) => {
            let _ = ready.send(Err(format!("Could not listen to the virtual cable: {e}")));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    unsafe {
        'run: while !shared.stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(5));
            loop {
                match capture.capture.GetNextPacketSize() {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(_) => break 'run,
                }
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                if capture.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None).is_err() {
                    break 'run;
                }
                let count = frames as usize * CH;
                let samples: Vec<f32> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    vec![0.0; count]
                } else {
                    std::slice::from_raw_parts(data as *const f32, count).to_vec()
                };
                let _ = capture.capture.ReleaseBuffer(frames);

                let mut ring = shared.ring.lock().unwrap();
                ring.extend(samples);
                if ring.len() > MAX_DEPTH_MS * RATE / 1000 * CH {
                    let drop = ring.len() - TRIM_TO_MS * RATE / 1000 * CH;
                    ring.drain(..drop);
                }
            }
        }
        let _ = capture.client.Stop();
    }
}

struct Render {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
}

fn render_init(id: &str) -> WinResult<Render> {
    unsafe {
        let device = enumerator()?.GetDevice(&HSTRING::from(id))?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK | FLAGS_CONVERT, 1_000_000, 0, &fmt(), None)?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let render: IAudioRenderClient = client.GetService()?;
        let buffer_frames = client.GetBufferSize()?;
        let _ = render.GetBuffer(buffer_frames)?;
        render.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        client.Start()?;
        Ok(Render { client, render, event, buffer_frames })
    }
}

fn render_thread(id: &str, shared: &Shared, ready: mpsc::Sender<Result<(), String>>) {
    if com_init().is_err() {
        let _ = ready.send(Err("Could not start the audio system.".into()));
        return;
    }
    let render = match render_init(id) {
        Ok(render) => render,
        Err(e) => {
            let _ = ready.send(Err(format!("Could not play to the output device: {e}")));
            return;
        }
    };
    let _ = ready.send(Ok(()));

    unsafe {
        let mut armed = false; // playback starts once the ring has filled once
        while !shared.stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(render.event, 200) != WAIT_OBJECT_0 {
                continue;
            }
            let Ok(padding) = render.client.GetCurrentPadding() else { break };
            let available = render.buffer_frames - padding;
            if available == 0 {
                continue;
            }
            let Ok(data) = render.render.GetBuffer(available) else { break };
            let out = std::slice::from_raw_parts_mut(data as *mut f32, available as usize * CH);

            let mut ring = shared.ring.lock().unwrap();
            if !armed && ring.len() >= PREBUFFER_MS * RATE / 1000 * CH {
                armed = true;
            }
            if armed {
                let mut ran_dry = false;
                for sample in out.iter_mut() {
                    *sample = ring.pop_front().unwrap_or_else(|| {
                        ran_dry = true;
                        0.0
                    });
                }
                if ran_dry {
                    shared.underruns.fetch_add(1, Ordering::Relaxed);
                    armed = false; // rebuffer
                }
            } else {
                out.fill(0.0);
            }
            drop(ring);
            let _ = render.render.ReleaseBuffer(available, 0);
        }
        let _ = render.client.Stop();
        let _ = CloseHandle(render.event);
    }
}
