//! Usage:
//!   cable_test --list
//!   cable_test [--capture "CABLE Input"] [--out "<part of device name>"] [--secs 60]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Result, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

const RATE: usize = 48_000;
const CHANNELS: usize = 2;
const PREBUFFER_MS: usize = 100;
const MAX_DEPTH_MS: usize = 400; // clock drift guard
const TRIM_TO_MS: usize = 150;

fn fmt() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: 3, // WAVE_FORMAT_IEEE_FLOAT
        nChannels: CHANNELS as u16,
        nSamplesPerSec: RATE as u32,
        nAvgBytesPerSec: (RATE * CHANNELS * 4) as u32,
        nBlockAlign: (CHANNELS * 4) as u16,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

const FLAGS_CONVERT: u32 = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

#[derive(Default)]
struct Stats {
    in_sq: f64,
    in_n: u64,
    underruns: u64,
    trims: u64,
    min_depth: usize,
    max_depth: usize,
}

struct Shared {
    ring: Mutex<VecDeque<f32>>,
    stats: Mutex<Stats>,
    stop: AtomicBool,
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// (id, friendly name) of every active output device.
fn outputs(enumr: &IMMDeviceEnumerator) -> Result<Vec<(String, String)>> {
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

fn capture_thread(id: String, sh: Arc<Shared>) -> Result<()> {
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
                let n = frames as usize * CHANNELS;
                let samples: Vec<f32> = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    vec![0.0; n]
                } else {
                    std::slice::from_raw_parts(data as *const f32, n).to_vec()
                };
                cap.ReleaseBuffer(frames)?;

                let sq = samples.iter().map(|&s| (s as f64) * (s as f64)).sum::<f64>();
                let (depth, trimmed) = {
                    let mut ring = sh.ring.lock().unwrap();
                    ring.extend(samples);
                    let mut trimmed = false;
                    if ring.len() > MAX_DEPTH_MS * RATE / 1000 * CHANNELS {
                        let keep = TRIM_TO_MS * RATE / 1000 * CHANNELS;
                        let drop = ring.len() - keep;
                        ring.drain(..drop);
                        trimmed = true;
                    }
                    (ring.len(), trimmed)
                };
                let mut st = sh.stats.lock().unwrap();
                st.in_sq += sq;
                st.in_n += n as u64;
                st.max_depth = st.max_depth.max(depth);
                if trimmed {
                    st.trims += 1;
                }
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}

fn render_thread(id: String, sh: Arc<Shared>) -> Result<()> {
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
        // Start with a buffer of silence so the first callback has room.
        let _ = render.GetBuffer(buffer_frames)?;
        render.ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
        client.Start()?;

        let mut armed = false; // true once the ring has filled once
        while !sh.stop.load(Ordering::Relaxed) {
            if WaitForSingleObject(event, 200) != WAIT_OBJECT_0 {
                continue;
            }
            let avail = buffer_frames - client.GetCurrentPadding()?;
            if avail == 0 {
                continue;
            }
            let data = render.GetBuffer(avail)?;
            let out = std::slice::from_raw_parts_mut(data as *mut f32, avail as usize * CHANNELS);

            let (depth, short) = {
                let mut ring = sh.ring.lock().unwrap();
                if !armed && ring.len() >= PREBUFFER_MS * RATE / 1000 * CHANNELS {
                    armed = true;
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
            }
        }
        let _ = client.Stop();
    }
    Ok(())
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn main() -> Result<()> {
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
    let secs: u64 = arg(&args, "--secs").and_then(|s| s.parse().ok()).unwrap_or(60);

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
    println!("Running for {secs} s, Ctrl+C to stop.\n");

    let sh = Arc::new(Shared {
        ring: Mutex::new(VecDeque::new()),
        stats: Mutex::new(Stats::default()),
        stop: AtomicBool::new(false),
    });
    let s = sh.clone();
    ctrlc::set_handler(move || s.stop.store(true, Ordering::Relaxed)).expect("ctrl-c handler");

    let (c, o) = (cap_id.clone(), out_id.clone());
    let (s1, s2) = (sh.clone(), sh.clone());
    let t1 = std::thread::spawn(move || capture_thread(c, s1));
    let t2 = std::thread::spawn(move || render_thread(o, s2));

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(secs) && !sh.stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));
        let depth_now = sh.ring.lock().unwrap().len() * 1000 / (RATE * CHANNELS);
        let mut st = sh.stats.lock().unwrap();
        let rms = (st.in_sq / st.in_n.max(1) as f64).sqrt();
        println!(
            "t={:>3}s  in {:6.1} dBFS  ring {:>3} ms (min {:>3}, max {:>3})  underruns {}  trims {}",
            start.elapsed().as_secs(),
            20.0 * rms.max(1e-9).log10(),
            depth_now,
            st.min_depth * 1000 / (RATE * CHANNELS),
            st.max_depth * 1000 / (RATE * CHANNELS),
            st.underruns,
            st.trims
        );
        st.in_sq = 0.0;
        st.in_n = 0;
    }
    sh.stop.store(true, Ordering::Relaxed);
    for (name, t) in [("capture", t1), ("render", t2)] {
        match t.join() {
            Ok(Err(e)) => eprintln!("{name} thread error: {e}"),
            Err(_) => eprintln!("{name} thread panicked"),
            _ => {}
        }
    }
    Ok(())
}
