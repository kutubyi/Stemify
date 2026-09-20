//! Usage:
//!   stem_pitch_test --list
//!   stem_pitch_test [--mix backing] [--semitones 0] [--hop 0.4] [--lookahead 0.12] [--xfade 0.05] [--cushion 0.25]
//!             [--capture "CABLE Input"] [--out "<part of device name>"] [--secs 60] [--cpu]
//!             [--model E:\Stemify-data\models\htdemucs_6s.onnx]
//!             [--ort-root E:\Stemify-data\venv-cuda\Lib\site-packages]
//!
//! Mix: comma list of drums,bass,other,vocals,guitar,piano, or `backing`
//! (everything but vocals), or `all`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ort::ep;
use ort::session::{builder::GraphOptimizationLevel, Session};
use ort::value::Tensor;
use rubberband::{Options, Stretcher};
use windows::core::HSTRING;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

type Res<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const RATE: usize = 44_100; 
const CH: usize = 2;
const WIN: usize = 343_980; // 7.8 s
const KEEP_FRAMES: usize = WIN + 3 * RATE; // input history 
const STEMS: [&str; 6] = ["drums", "bass", "other", "vocals", "guitar", "piano"];
const ALL: u32 = 1 << 8; // pass the input through
const BACKING: u32 = 0b110111; // everything but vocals

fn oe<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn parse_mix(s: &str) -> Option<u32> {
    let mut mask = 0;
    for part in s.split(',').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()) {
        match part.as_str() {
            "all" => return Some(ALL),
            "backing" => mask |= BACKING,
            name => mask |= 1 << STEMS.iter().position(|s| *s == name)?,
        }
    }
    (mask != 0).then_some(mask)
}

fn mix_name(mask: u32) -> String {
    if mask & ALL != 0 {
        return "all".into();
    }
    STEMS.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, n)| *n).collect::<Vec<_>>().join("+")
}

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

struct Hist {
    buf: VecDeque<f32>,
    base: u64,  // absolute frame number of buf[0]
    total: u64, // frames captured so far
}

#[derive(Default)]
struct Stats {
    in_sq: f64,
    in_n: u64,
    inf_sum: f64,
    inf_n: u32,
    inf_max: f64,
    steps: u64,
    behind: u64,
    underruns: u64,
    min_depth: usize,
    max_depth: usize,
}

struct Shared {
    hist: Mutex<Hist>,
    out: Mutex<VecDeque<f32>>,
    stats: Mutex<Stats>,
    stop: AtomicBool,
    ready: AtomicBool,
    mask: AtomicU32,
    hop: usize,     // frames
    cushion: usize, // frames
}

fn enumerator() -> windows::core::Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// (id, friendly name) of every active output device.
fn outputs(enumr: &IMMDeviceEnumerator) -> windows::core::Result<Vec<(String, String)>> {
    let mut v = Vec::new();
    unsafe {
        let coll = enumr.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            let id = d.GetId()?.to_string()?;
            let pv = d.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?;
            v.push((id, pv.to_string()));
        }
    }
    Ok(v)
}

fn init_ort(root: &Path, cpu: bool) -> Res<()> {
    let dirs = [
        root.join("nvidia").join("cu13").join("bin").join("x86_64"),
        root.join("nvidia").join("cudnn").join("bin"),
        root.join("onnxruntime").join("capi"),
    ];
    let mut paths: Vec<PathBuf> = dirs.to_vec();
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
    std::env::set_var("PATH", std::env::join_paths(paths)?);

    let dll = root.join("onnxruntime").join("capi").join("onnxruntime.dll");
    let mut builder = ort::init_from(&dll).map_err(oe)?;
    if !cpu {
        builder = builder.with_execution_providers([ep::CUDA::default().build().error_on_failure()]);
    }
    builder.commit();
    Ok(())
}

fn capture_thread(id: String, sh: Arc<Shared>) -> Res<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let dev = enumerator()?.GetDevice(&HSTRING::from(id))?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | FLAGS_CONVERT,
            2_000_000,
            0,
            &fmt(),
            None,
        )?;
        let cap: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        while !sh.stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(5));
            while cap.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
                cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                let n = frames as usize * CH;
                let samples: Vec<f32> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    vec![0.0; n]
                } else {
                    std::slice::from_raw_parts(data as *const f32, n).to_vec()
                };
                cap.ReleaseBuffer(frames)?;

                let sq = samples.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>();
                {
                    let mut h = sh.hist.lock().unwrap();
                    h.buf.extend(samples);
                    h.total += frames as u64;
                    if h.buf.len() > KEEP_FRAMES * CH {
                        let drop = h.buf.len() - KEEP_FRAMES * CH;
                        h.buf.drain(..drop);
                        h.base += (drop / CH) as u64;
                    }
                }
                let mut st = sh.stats.lock().unwrap();
                st.in_sq += sq;
                st.in_n += n as u64;
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}

const RB_CHUNK: usize = 4096; 

struct Shifter {
    rb: Stretcher,
    discard: usize, 
}

impl Shifter {
    fn new(semitones: f64) -> Self {
        let mut rb = Stretcher::new(
            RATE as u32,
            CH as u32,
            Options::PROCESS_REALTIME | Options::ENGINE_FINER | Options::CHANNELS_TOGETHER,
            1.0,
            2f64.powf(semitones / 12.0),
        );
        rb.set_max_process_size(RB_CHUNK as u32);
        let pad = rb.preferred_start_pad() as usize;
        let discard = rb.start_delay() as usize;
        println!(
            "Pitch engine: engine R{}, start pad {} ms, start delay {} ms",
            rb.engine_version(),
            pad * 1000 / RATE,
            discard * 1000 / RATE
        );
        let mut s = Shifter { rb, discard };
        s.run(&vec![0f32; pad * CH]);
        s
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
                let avail = self.rb.available().unwrap_or(0) as usize;
                if avail == 0 {
                    break;
                }
                let take = avail.min(RB_CHUNK);
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

fn worker_thread(model: PathBuf, look: usize, xf: usize, semitones: f64, sh: Arc<Shared>) -> Res<()> {
    let mut session = Session::builder()
        .map_err(oe)?
        .with_optimization_level(GraphOptimizationLevel::Level3)
        .map_err(oe)?
        .commit_from_file(&model)
        .map_err(oe)?;

    // The first CUDA runs are slow (kernel selection); do them before going live.
    for i in 0..2 {
        let t = Instant::now();
        let tensor = Tensor::from_array(([1usize, CH, WIN], vec![0f32; CH * WIN])).map_err(oe)?;
        let _ = session.run(ort::inputs!["mix" => tensor]).map_err(oe)?;
        println!("model warm-up {}: {} ms", i + 1, t.elapsed().as_millis());
    }
    sh.ready.store(true, Ordering::Relaxed);

    let hop = sh.hop;
    let need = hop + xf + look; // frames from block start to the end of the window
    assert!(need < WIN, "hop + xfade + lookahead must be shorter than 7.8 s");
    let mut tail: Option<Vec<f32>> = None;
    let mut k: u64 = 0;

    let mut shifter = (semitones != 0.0).then(|| Shifter::new(semitones));

    'outer: loop {
        let s = k * hop as u64; // absolute frame where this block starts
        let e = s + need as u64; // absolute frame where the window ends
        loop {
            if sh.stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            if sh.hist.lock().unwrap().total >= e {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        let mask = sh.mask.load(Ordering::Relaxed);
        let t_run = Instant::now();
        // `block` = hop + xf frames, interleaved, starting at absolute frame `s`.
        let block: Vec<f32> = if mask & ALL != 0 {
            let mut b = vec![0f32; (hop + xf) * CH];
            let h = sh.hist.lock().unwrap();
            for j in 0..hop + xf {
                let abs = s as i64 + j as i64 - h.base as i64;
                if abs >= 0 && (abs as usize + 1) * CH <= h.buf.len() {
                    b[j * CH] = h.buf[abs as usize * CH];
                    b[j * CH + 1] = h.buf[abs as usize * CH + 1];
                }
            }
            b
        } else {
            let mut input = vec![0f32; CH * WIN];
            {
                let h = sh.hist.lock().unwrap();
                let start = e as i64 - WIN as i64;
                for i in 0..WIN {
                    let abs = start + i as i64 - h.base as i64;
                    if abs >= 0 && (abs as usize + 1) * CH <= h.buf.len() {
                        input[i] = h.buf[abs as usize * CH];
                        input[WIN + i] = h.buf[abs as usize * CH + 1];
                    }
                }
            }
            let tensor = Tensor::from_array(([1usize, CH, WIN], input)).map_err(oe)?;
            let outputs = session.run(ort::inputs!["mix" => tensor]).map_err(oe)?;
            let (_, stems) = outputs["stems"].try_extract_tensor::<f32>().map_err(oe)?;

            let first = WIN - need; // window index of the block start
            let mut b = vec![0f32; (hop + xf) * CH];
            for st in (0..STEMS.len()).filter(|st| mask & (1 << st) != 0) {
                for c in 0..CH {
                    let src = &stems[(st * CH + c) * WIN + first..(st * CH + c) * WIN + first + hop + xf];
                    for (j, v) in src.iter().enumerate() {
                        b[j * CH + c] += v;
                    }
                }
            }
            b
        };
        let infer_ms = t_run.elapsed().as_secs_f64() * 1000.0;

        let mut emit: Vec<f32> = Vec::with_capacity(hop * CH);
        match &tail {
            Some(t) => {
                for j in 0..xf {
                    let w = j as f32 / xf as f32;
                    for c in 0..CH {
                        emit.push(t[j * CH + c] * (1.0 - w) + block[j * CH + c] * w);
                    }
                }
            }
            None => emit.extend_from_slice(&block[..xf * CH]),
        }
        emit.extend_from_slice(&block[xf * CH..hop * CH]);
        tail = Some(block[hop * CH..(hop + xf) * CH].to_vec());
        for v in emit.iter_mut() {
            *v = v.clamp(-1.0, 1.0);
        }
        let emit = match shifter.as_mut() {
            Some(s) => s.run(&emit),
            None => emit,
        };
        sh.out.lock().unwrap().extend(emit);

        let total_now = sh.hist.lock().unwrap().total;
        let mut st = sh.stats.lock().unwrap();
        st.steps += 1;
        st.inf_sum += infer_ms;
        st.inf_n += 1;
        st.inf_max = st.inf_max.max(infer_ms);
        if total_now > e + hop as u64 {
            st.behind += 1; // finished more than a hop after the window closed
        }
        k += 1;
    }
    Ok(())
}

fn render_thread(id: String, sh: Arc<Shared>) -> Res<()> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let dev = enumerator()?.GetDevice(&HSTRING::from(id))?;
        let client: IAudioClient = dev.Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | FLAGS_CONVERT,
            1_000_000, // 100 ms
            0,
            &fmt(),
            None,
        )?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        let render: IAudioRenderClient = client.GetService()?;
        let buffer_frames = client.GetBufferSize()?;
        let _ = render.GetBuffer(buffer_frames)?;
        render.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        client.Start()?;

        let arm_at = sh.hop * CH * 6 / 10;  
        let mut armed = false;
        while !sh.stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }
            let avail = buffer_frames - client.GetCurrentPadding()?;
            if avail == 0 {
                continue;
            }
            let data = render.GetBuffer(avail)?;
            let out = std::slice::from_raw_parts_mut(data as *mut f32, avail as usize * CH);

            let (depth, short) = {
                let mut ring = sh.out.lock().unwrap();
                if !armed && ring.len() >= arm_at {
                    armed = true;
                    for _ in 0..sh.cushion * CH {
                        ring.push_front(0.0);
                    }
                }
                let mut short = false;
                if armed {
                    for s in out.iter_mut() {
                        *s = ring.pop_front().unwrap_or_else(|| {
                            short = true;
                            0.0
                        });
                    }
                } else {
                    out.fill(0.0);
                }
                (ring.len(), short)
            };
            render.ReleaseBuffer(avail, 0)?;

            let mut st = sh.stats.lock().unwrap();
            if short {
                st.underruns += 1;
                armed = false; // rebuffer
            } else if armed {
                st.min_depth = if st.min_depth == 0 { depth } else { st.min_depth.min(depth) };
                st.max_depth = st.max_depth.max(depth);
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn secs_arg(args: &[String], name: &str, default: f64) -> f64 {
    arg(args, name).and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn main() -> Res<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let args: Vec<String> = std::env::args().collect();
    let enumr = enumerator()?;
    let devices = outputs(&enumr)?;

    if args.iter().any(|a| a == "--list") {
        println!("Active output devices:");
        for (_, name) in &devices {
            println!("  {name}");
        }
        return Ok(());
    }

    let cap_pat = arg(&args, "--capture").unwrap_or_else(|| "CABLE Input".into()).to_lowercase();
    let secs = secs_arg(&args, "--secs", 60.0) as u64;
    let hop = (secs_arg(&args, "--hop", 0.4) * RATE as f64) as usize;
    let look = (secs_arg(&args, "--lookahead", 0.12) * RATE as f64) as usize;
    let xf = (secs_arg(&args, "--xfade", 0.05) * RATE as f64) as usize;
    let cushion = (secs_arg(&args, "--cushion", 0.25) * RATE as f64) as usize;
    let cpu = args.iter().any(|a| a == "--cpu");
    let semitones = secs_arg(&args, "--semitones", 0.0);
    let model = PathBuf::from(arg(&args, "--model").unwrap_or_else(|| r"E:\Stemify-data\models\htdemucs_6s.onnx".into()));
    let ort_root = PathBuf::from(
        arg(&args, "--ort-root").unwrap_or_else(|| r"E:\Stemify-data\venv-cuda\Lib\site-packages".into()),
    );
    let Some(mask) = parse_mix(&arg(&args, "--mix").unwrap_or_else(|| "backing".into())) else {
        eprintln!("Bad --mix. Use e.g. vocals, drums,bass, backing or all.");
        std::process::exit(1);
    };

    let Some((cap_id, cap_name)) = devices.iter().find(|(_, n)| n.to_lowercase().contains(&cap_pat)) else {
        eprintln!("No output device matching \"{cap_pat}\". Run with --list to see names.");
        std::process::exit(1);
    };
    let (out_id, out_name) = match arg(&args, "--out") {
        Some(p) => {
            let p = p.to_lowercase();
            match devices.iter().find(|(_, n)| n.to_lowercase().contains(&p)) {
                Some(d) => d.clone(),
                None => {
                    eprintln!("No output device matching \"{p}\". Run with --list.");
                    std::process::exit(1);
                }
            }
        }
        None => unsafe {
            let d = enumr.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let id = d.GetId()?.to_string()?;
            let name = devices.iter().find(|(i, _)| *i == id).map(|d| d.1.clone()).unwrap_or_default();
            (id, name)
        },
    };
    if *cap_id == out_id {
        eprintln!("Capture and output are the same device (\"{cap_name}\"): that would feed back. Pick a different --out.");
        std::process::exit(1);
    }

    println!("Capturing : {cap_name}");
    println!("Playing to: {out_name}");
    println!(
        "Model     : {}  ({})",
        model.display(),
        if cpu { "CPU" } else { "CUDA" }
    );
    println!(
        "Mix       : {}   hop {:.2}s  lookahead {:.2}s  xfade {:.2}s  cushion {:.2}s",
        mix_name(mask),
        hop as f64 / RATE as f64,
        look as f64 / RATE as f64,
        xf as f64 / RATE as f64,
        cushion as f64 / RATE as f64
    );
    println!("Pitch     : {semitones:+} semitones{}", if semitones == 0.0 { " (shifter off)" } else { "" });
    println!("Type a new mix + Enter while running (e.g. vocals, drums,bass, backing, all). Ctrl+C to stop.\n");

    init_ort(&ort_root, cpu)?;

    let sh = Arc::new(Shared {
        hist: Mutex::new(Hist { buf: VecDeque::new(), base: 0, total: 0 }),
        out: Mutex::new(VecDeque::new()),
        stats: Mutex::new(Stats::default()),
        stop: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        mask: AtomicU32::new(mask),
        hop,
        cushion,
    });
    let s = sh.clone();
    ctrlc::set_handler(move || s.stop.store(true, Ordering::Relaxed)).expect("ctrl-c handler");

    println!("Loading model...");
    let s = sh.clone();
    let worker = std::thread::spawn(move || worker_thread(model, look, xf, semitones, s));
    while !sh.ready.load(Ordering::Relaxed) {
        if worker.is_finished() {
            return match worker.join() {
                Ok(Err(e)) => Err(e),
                Ok(Ok(())) => Err("worker exited early".into()),
                Err(_) => Err("worker panicked".into()),
            };
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("Model ready. Now switch Spotify's output to \"{cap_name}\".\n");

    let (c, o) = (cap_id.clone(), out_id.clone());
    let (s1, s2, s3) = (sh.clone(), sh.clone(), sh.clone());
    let t_cap = std::thread::spawn(move || capture_thread(c, s1));
    let t_ren = std::thread::spawn(move || render_thread(o, s2));
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            match parse_mix(&line) {
                Some(m) => {
                    s3.mask.store(m, Ordering::Relaxed);
                    println!("mix -> {}", mix_name(m));
                }
                None => println!("?  try: vocals   drums,bass   backing   all"),
            }
        }
    });

    let ms = |samples: usize| samples * 1000 / (RATE * CH);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) && !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        let depth_now = ms(sh.out.lock().unwrap().len());
        let mix = mix_name(sh.mask.load(Ordering::Relaxed));
        let mut st = sh.stats.lock().unwrap();
        let rms = (st.in_sq / st.in_n.max(1) as f64).sqrt();
        let avg = if st.inf_n > 0 { st.inf_sum / st.inf_n as f64 } else { 0.0 };
        println!(
            "t={:>3}s in {:6.1} dBFS | {:<18} | infer avg {:>4.0} max {:>4.0} ms  steps {}  behind {} | ring {:>4} ms (min {:>4}, max {:>4})  underruns {}",
            start.elapsed().as_secs(),
            20.0 * rms.max(1e-9).log10(),
            mix,
            avg,
            st.inf_max,
            st.steps,
            st.behind,
            depth_now,
            ms(st.min_depth),
            ms(st.max_depth),
            st.underruns
        );
        st.in_sq = 0.0;
        st.in_n = 0;
        st.inf_sum = 0.0;
        st.inf_n = 0;
        st.inf_max = 0.0;
    }
    sh.stop.store(true, Ordering::Relaxed);
    for (name, t) in [("worker", worker), ("capture", t_cap), ("render", t_ren)] {
        match t.join() {
            Ok(Err(e)) => eprintln!("{name} thread error: {e}"),
            Err(_) => eprintln!("{name} thread panicked"),
            _ => {}
        }
    }
    Ok(())
}
