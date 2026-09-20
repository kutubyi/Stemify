//! Usage:
//!   routing_test get                     show Spotify's persisted output device
//!   routing_test set "CABLE Input"       route Spotify to that device
//!   routing_test clear                   put Spotify back on the default device
//!   routing_test --list                  list output devices

use std::ffi::c_void;

use windows::core::{Interface, Result, GUID, HRESULT, HSTRING, IUnknown};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::WinRT::RoGetActivationFactory;

// Interface ids
const IID_21H2: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
const IID_DOWNLEVEL: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

const FLOW_RENDER: i32 = 0; // EDataFlow::eRender
const ROLES: [(&str, i32); 2] = [("console", 0), ("multimedia", 1)]; // ERole

#[repr(C)]
struct PolicyVtbl {
    _iunknown: [usize; 3],
    _iinspectable: [usize; 3],
    _unused: [usize; 19],
    set_persisted: unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut c_void) -> HRESULT,
    get_persisted: unsafe extern "system" fn(*mut c_void, u32, i32, i32, *mut *mut c_void) -> HRESULT,
    _clear_all: usize, // deliberately never called
}

struct Policy {
    _factory: IUnknown, // keeps the object alive
    obj: *mut c_void,
    variant: &'static str,
}

impl Policy {
    fn open() -> Result<Self> {
        unsafe {
            let factory: IUnknown = RoGetActivationFactory(&HSTRING::from("Windows.Media.Internal.AudioPolicyConfig"))?;
            for (iid, variant) in [(IID_21H2, "21H2+"), (IID_DOWNLEVEL, "downlevel")] {
                let mut obj: *mut c_void = std::ptr::null_mut();
                if factory.query(&iid, &mut obj).is_ok() && !obj.is_null() {
                    return Ok(Self { _factory: factory, obj, variant });
                }
            }
        }
        Err(windows::core::Error::new(
            HRESULT(0x80004002u32 as i32), // E_NOINTERFACE
            "AudioPolicyConfig does not expose either known interface on this Windows build",
        ))
    }

    fn vtbl(&self) -> &PolicyVtbl {
        unsafe { &**(self.obj as *mut *const PolicyVtbl) }
    }

    /// `device` = None resets that app to the default device.
    fn set(&self, pid: u32, role: i32, device: Option<&str>) -> HRESULT {
        let hs = device.map(|d| HSTRING::from(device_path(d)));
        let raw: *mut c_void = match &hs {
            Some(h) => unsafe { std::mem::transmute_copy::<HSTRING, *mut c_void>(h) },
            None => std::ptr::null_mut(),
        };
        unsafe { (self.vtbl().set_persisted)(self.obj, pid, FLOW_RENDER, role, raw) }
    }

    fn get(&self, pid: u32, role: i32) -> (HRESULT, String) {
        let mut out: *mut c_void = std::ptr::null_mut();
        let hr = unsafe { (self.vtbl().get_persisted)(self.obj, pid, FLOW_RENDER, role, &mut out) };
        let s = unsafe { std::mem::transmute::<*mut c_void, HSTRING>(out) }; // takes ownership, frees it
        (hr, s.to_string())
    }
}

fn device_path(endpoint_id: &str) -> String {
    format!(r"\\?\SWD#MMDEVAPI#{endpoint_id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
}

fn enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
}

/// (id, friendly name) of every active output device.
fn outputs() -> Result<Vec<(String, String)>> {
    let mut v = Vec::new();
    unsafe {
        let coll = enumerator()?.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            let id = d.GetId()?.to_string()?;
            let pv = d.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?;
            v.push((id, pv.to_string()));
        }
    }
    Ok(v)
}

/// Every Spotify.exe process id.
fn spotify_pids() -> Result<Vec<u32>> {
    let mut found = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let mut entry = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        let mut ok = Process32FirstW(snap, &mut entry).is_ok();
        while ok {
            let len = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            if String::from_utf16_lossy(&entry.szExeFile[..len]).eq_ignore_ascii_case("spotify.exe") {
                found.push(entry.th32ProcessID);
            }
            ok = Process32NextW(snap, &mut entry).is_ok();
        }
        let _ = CloseHandle(snap);
    }
    Ok(found)
}

fn show(policy: &Policy, pids: &[u32], devices: &[(String, String)]) {
    for &pid in pids {
        for (role_name, role) in ROLES {
            let (hr, path) = policy.get(pid, role);
            let what = if hr.is_err() {
                format!("error {hr:?}")
            } else if path.is_empty() {
                "(default device)".to_string()
            } else {
                match devices.iter().find(|(id, _)| path.contains(id.as_str())) {
                    Some((_, name)) => format!("{name}\n{:>30}{path}", ""),
                    None => path,
                }
            };
            println!("  pid {pid:>6} {role_name:<10} -> {what}");
        }
    }
}

fn main() -> Result<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let devices = outputs()?;
    let cmd = args.first().map(String::as_str).unwrap_or("");

    if cmd == "--list" || cmd.is_empty() {
        println!("Active output devices:");
        for (_, name) in &devices {
            println!("  {name}");
        }
        println!("\nUsage: routing_test get | set \"CABLE Input\" | clear");
        return Ok(());
    }

    let pids = spotify_pids()?;
    if pids.is_empty() {
        eprintln!("Spotify.exe is not running.");
        std::process::exit(1);
    }
    let policy = Policy::open()?;
    println!("AudioPolicyConfig opened (interface variant: {}). Spotify processes: {}\n", policy.variant, pids.len());

    match cmd {
        "get" => {
            println!("Spotify's persisted output device:");
            show(&policy, &pids, &devices);
        }
        "set" => {
            let pat = args.get(1).map(|s| s.to_lowercase()).unwrap_or_default();
            let Some((id, name)) = devices.iter().find(|(_, n)| !pat.is_empty() && n.to_lowercase().contains(&pat)) else {
                eprintln!("No output device matching \"{pat}\". Run --list.");
                std::process::exit(1);
            };
            println!("Routing Spotify to: {name}");
            for &pid in &pids {
                for (role_name, role) in ROLES {
                    println!("  set pid {pid} {role_name}: {:?}", policy.set(pid, role, Some(id)));
                }
            }
            println!("\nNow:");
            show(&policy, &pids, &devices);
        }
        "clear" => {
            println!("Putting Spotify back on the default device");
            for &pid in &pids {
                for (role_name, role) in ROLES {
                    println!("  clear pid {pid} {role_name}: {:?}", policy.set(pid, role, None));
                }
            }
            println!("\nNow:");
            show(&policy, &pids, &devices);
        }
        _ => eprintln!("Unknown command. Use: get | set \"CABLE Input\" | clear | --list"),
    }
    Ok(())
}
