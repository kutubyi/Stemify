use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rubberband::{Options, Stretcher};
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::routing::{com_init, outputs};
use crate::separator::{Separator, ALL_MASK, STEMS, WIN};

type WinResult<T> = windows::core::Result<T>;

const RATE: usize = 44_100;
const CH: usize = 2;
const RB_CHUNK: usize = 4096;

const HOP: usize = RATE * 2 / 5; // 0.4 s
const LOOKAHEAD: usize = RATE; // 1 s
const XFADE: usize = RATE / 20; // 0.05 s
const CUSHION: usize = RATE / 4; // 0.25 s
const KEEP_FRAMES: usize = WIN + 3 * RATE;

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
struct Hist {
    buf: VecDeque<f32>,
    base: u64,
    total: u64,
}

struct Shared {
    hist: Mutex<Hist>,
    ring: Mutex<VecDeque<f32>>,
    stop: AtomicBool,
    underruns: AtomicU64,
    late: AtomicU64,
    semitones: AtomicI32,
    stems: AtomicU32,
    windowed: bool,
    arm_frames: usize,
    cushion_frames: usize,
    max_ring_frames: usize,
    trim_ring_frames: usize,
}

impl Shared {
    fn new(semitones: i32, stems: u32, windowed: bool) -> Shared {
        Shared {
            hist: Mutex::default(),
            ring: Mutex::default(),
            stop: AtomicBool::new(false),
            underruns: AtomicU64::new(0),
            late: AtomicU64::new(0),
            semitones: AtomicI32::new(semitones),
            stems: AtomicU32::new(stems),
            windowed,
            arm_frames: if windowed { HOP * 3 / 5 } else { RATE / 10 },
            cushion_frames: if windowed { CUSHION } else { 0 },
            max_ring_frames: if windowed { RATE * 5 / 2 } else { RATE * 2 / 5 },
            trim_ring_frames: if windowed { CUSHION + HOP } else { RATE * 3 / 20 },
        }
    }
}

pub struct Pipeline {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
    output_id: String,
}

impl Pipeline {
    pub fn start(semitones: i32, stems: u32, separator: Option<Arc<Mutex<Separator>>>) -> Result<Pipeline, String> {
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

        let shared = Arc::new(Shared::new(semitones, stems, separator.is_some()));
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let mut threads = Vec::new();
        {
            let (shared, tx, id) = (shared.clone(), tx.clone(), cable.0);
            threads.push(thread::spawn(move || capture_thread(&id, &shared, tx)));
        }
        {
            let (shared, tx) = (shared.clone(), tx.clone());
            threads.push(thread::spawn(move || processor_thread(&shared, separator, tx)));
        }
        {
            let (shared, id) = (shared.clone(), output_id.clone());
            threads.push(thread::spawn(move || render_thread(&id, &shared, tx)));
        }

        let pipeline = Pipeline { shared, threads, output_id };
        for _ in 0..3 {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Ok(())) => {}
                Ok(Err(message)) => return Err(message),
                Err(_) => return Err("Timed out starting the audio.".into()),
            }
        }
        Ok(pipeline)
    }

    pub fn healthy(&self) -> bool {
        self.threads.iter().all(|thread| !thread.is_finished()) && default_output_id().is_ok_and(|id| id == self.output_id)
    }

    pub fn is_windowed(&self) -> bool {
        self.shared.windowed
    }

    pub fn set_semitones(&self, semitones: i32) {
        self.shared.semitones.store(semitones, Ordering::Relaxed);
    }

    pub fn set_stems(&self, stems: u32) {
        self.shared.stems.store(stems, Ordering::Relaxed);
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        eprintln!(
            "[audio] pipeline stopped ({} underruns, {} late blocks)",
            self.shared.underruns.load(Ordering::Relaxed),
            self.shared.late.load(Ordering::Relaxed)
        );
    }
}

fn enumerator() -> WinResult<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

fn default_output_id() -> WinResult<String> {
    unsafe { Ok(enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole)?.GetId()?.to_string()?) }
}

fn pitch_scale(semitones: i32) -> f64 {
    2f64.powf(semitones as f64 / 12.0)
}

struct Shifter {
    rb: Stretcher,
    semitones: i32,
    discard: usize,
}

impl Shifter {
    fn new(semitones: i32) -> Self {
        let mut rb = Stretcher::new(
            RATE as u32,
            CH as u32,
            Options::PROCESS_REALTIME | Options::ENGINE_FINER | Options::CHANNELS_TOGETHER,
            1.0,
            pitch_scale(semitones),
        );
        rb.set_max_process_size(RB_CHUNK as u32);
        let pad = rb.preferred_start_pad() as usize;
        let discard = rb.start_delay() as usize;
        let mut shifter = Shifter { rb, semitones, discard };
        shifter.run(&vec![0f32; pad * CH]);
        shifter
    }

    fn set_semitones(&mut self, semitones: i32) {
        if semitones != self.semitones {
            self.rb.set_pitch_scale(pitch_scale(semitones));
            self.semitones = semitones;
        }
    }

    fn run(&mut self, input: &[f32]) -> Vec<f32> {
        let frames = input.len() / CH;
        let (mut l, mut r) = (vec![0f32; RB_CHUNK], vec![0f32; RB_CHUNK]);
        let (mut out_l, mut out_r) = (vec![0f32; RB_CHUNK], vec![0f32; RB_CHUNK]);
        let mut out = Vec::with_capacity(input.len());
        let mut pos = 0;
        while pos < frames {
            let n = RB_CHUNK.min(frames - pos);
            for i in 0..n {
                l[i] = input[(pos + i) * CH];
                r[i] = input[(pos + i) * CH + 1];
            }
            self.rb.process(&[&l[..n], &r[..n]], false);
            pos += n;
            loop {
                let available = self.rb.available().unwrap_or(0) as usize;
                if available == 0 {
                    break;
                }
                let take = available.min(RB_CHUNK);
                let got = self.rb.retrieve(&mut [&mut out_l[..take], &mut out_r[..take]]) as usize;
                let skip = self.discard.min(got);
                self.discard -= skip;
                for i in skip..got {
                    out.push(out_l[i]);
                    out.push(out_r[i]);
                }
            }
        }
        out
    }
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

                let mut hist = shared.hist.lock().unwrap();
                hist.buf.extend(samples);
                hist.total += frames as u64;
                if hist.buf.len() > KEEP_FRAMES * CH {
                    let drop = hist.buf.len() - KEEP_FRAMES * CH;
                    hist.buf.drain(..drop);
                    hist.base += (drop / CH) as u64;
                }
            }
        }
        let _ = capture.client.Stop();
    }
}

fn processor_thread(shared: &Shared, separator: Option<Arc<Mutex<Separator>>>, ready: mpsc::Sender<Result<(), String>>) {
    let mut shifter = Shifter::new(shared.semitones.load(Ordering::Relaxed));
    let _ = ready.send(Ok(()));
    match separator {
        Some(separator) => windowed_loop(shared, &mut shifter, &separator),
        None => simple_loop(shared, &mut shifter),
    }
}

fn push_ring(shared: &Shared, samples: Vec<f32>) {
    let mut ring = shared.ring.lock().unwrap();
    ring.extend(samples);
    if ring.len() > shared.max_ring_frames * CH {
        let drop = ring.len() - shared.trim_ring_frames * CH;
        ring.drain(..drop);
    }
}

fn simple_loop(shared: &Shared, shifter: &mut Shifter) {
    let mut next = shared.hist.lock().unwrap().total;
    while !shared.stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(5));
        let chunk: Vec<f32> = {
            let hist = shared.hist.lock().unwrap();
            if hist.total <= next {
                continue;
            }
            let from = (next.max(hist.base) - hist.base) as usize * CH;
            next = hist.total;
            hist.buf.range(from..).copied().collect()
        };
        shifter.set_semitones(shared.semitones.load(Ordering::Relaxed));
        push_ring(shared, shifter.run(&chunk));
    }
}

fn input_block(shared: &Shared, start: u64) -> Vec<f32> {
    let mut block = vec![0f32; (HOP + XFADE) * CH];
    let hist = shared.hist.lock().unwrap();
    for j in 0..HOP + XFADE {
        let abs = start as i64 + j as i64 - hist.base as i64;
        if abs >= 0 && (abs as usize + 1) * CH <= hist.buf.len() {
            block[j * CH] = hist.buf[abs as usize * CH];
            block[j * CH + 1] = hist.buf[abs as usize * CH + 1];
        }
    }
    block
}

fn separated_block(shared: &Shared, separator: &Mutex<Separator>, start: u64, end: u64, mask: u32) -> Result<Vec<f32>, String> {
    let mut window = vec![0f32; CH * WIN];
    {
        let hist = shared.hist.lock().unwrap();
        let first = end as i64 - WIN as i64;
        for i in 0..WIN {
            let abs = first + i as i64 - hist.base as i64;
            if abs >= 0 && (abs as usize + 1) * CH <= hist.buf.len() {
                window[i] = hist.buf[abs as usize * CH];
                window[WIN + i] = hist.buf[abs as usize * CH + 1];
            }
        }
    }
    let first = WIN - (end - start) as usize; 
    let mut block = vec![0f32; (HOP + XFADE) * CH];
    let subtract = mask.count_ones() > STEMS.len() as u32 / 2;
    if subtract {
        for c in 0..CH {
            for j in 0..HOP + XFADE {
                block[j * CH + c] = window[c * WIN + first + j];
            }
        }
    }
    let stems = separator.lock().unwrap().separate(window)?;
    for stem in (0..STEMS.len()).filter(|stem| (mask & (1 << stem) != 0) != subtract) {
        for c in 0..CH {
            let base = (stem * CH + c) * WIN + first;
            for (j, value) in stems[base..base + HOP + XFADE].iter().enumerate() {
                if subtract {
                    block[j * CH + c] -= value;
                } else {
                    block[j * CH + c] += value;
                }
            }
        }
    }
    Ok(block)
}

fn windowed_loop(shared: &Shared, shifter: &mut Shifter, separator: &Mutex<Separator>) {
    let need = (HOP + XFADE + LOOKAHEAD) as u64; 
    let origin = shared.hist.lock().unwrap().total;
    let mut tail: Option<Vec<f32>> = None;
    let mut k: u64 = 0;

    'run: loop {
        let start = origin + k * HOP as u64;
        let end = start + need;
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                break 'run;
            }
            if shared.hist.lock().unwrap().total >= end {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }

        let mask = shared.stems.load(Ordering::Relaxed);
        let block = if mask == ALL_MASK {
            input_block(shared, start)
        } else {
            separated_block(shared, separator, start, end, mask).unwrap_or_else(|message| {
                eprintln!("[audio] separation failed: {message}");
                input_block(shared, start)
            })
        };

        let mut emit: Vec<f32> = Vec::with_capacity(HOP * CH);
        match &tail {
            Some(tail) => {
                for j in 0..XFADE {
                    let w = j as f32 / XFADE as f32;
                    for c in 0..CH {
                        emit.push(tail[j * CH + c] * (1.0 - w) + block[j * CH + c] * w);
                    }
                }
            }
            None => emit.extend_from_slice(&block[..XFADE * CH]),
        }
        emit.extend_from_slice(&block[XFADE * CH..HOP * CH]);
        tail = Some(block[HOP * CH..(HOP + XFADE) * CH].to_vec());
        for value in emit.iter_mut() {
            *value = value.clamp(-1.0, 1.0);
        }

        shifter.set_semitones(shared.semitones.load(Ordering::Relaxed));
        push_ring(shared, shifter.run(&emit));

        if shared.hist.lock().unwrap().total > end + HOP as u64 {
            shared.late.fetch_add(1, Ordering::Relaxed);
        }
        k += 1;
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
        let mut armed = false; 
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
            if !armed && ring.len() >= shared.arm_frames * CH {
                armed = true;
                for _ in 0..shared.cushion_frames * CH {
                    ring.push_front(0.0);
                }
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
                    armed = false;
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
