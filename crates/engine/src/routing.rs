use std::ffi::c_void;

use windows::core::{Interface, GUID, HRESULT, HSTRING, IUnknown};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::WinRT::RoGetActivationFactory;

type WinResult<T> = windows::core::Result<T>;

const IID_21H2: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
const IID_DOWNLEVEL: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

const FLOW_RENDER: i32 = 0; // EDataFlow::eRender
const ROLES: [i32; 2] = [0, 1]; // ERole::eConsole, eMultimedia

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
    _factory: IUnknown, 
    obj: *mut c_void,
}

impl Policy {
    fn open() -> WinResult<Self> {
        unsafe {
            let factory: IUnknown = RoGetActivationFactory(&HSTRING::from("Windows.Media.Internal.AudioPolicyConfig"))?;
            for iid in [IID_21H2, IID_DOWNLEVEL] {
                let mut obj: *mut c_void = std::ptr::null_mut();
                if factory.query(&iid, &mut obj).is_ok() && !obj.is_null() {
                    return Ok(Self { _factory: factory, obj });
                }
            }
        }
        Err(windows::core::Error::new(
            HRESULT(0x80004002u32 as i32), // E_NOINTERFACE
            "AudioPolicyConfig does not expose a known interface on this Windows build",
        ))
    }

    fn vtbl(&self) -> &PolicyVtbl {
        unsafe { &**(self.obj as *mut *const PolicyVtbl) }
    }

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
        let s = unsafe { std::mem::transmute::<*mut c_void, HSTRING>(out) }; 
        (hr, s.to_string())
    }

    fn audio_pids(&self, pids: &[u32]) -> Vec<u32> {
        pids.iter().copied().filter(|&pid| self.get(pid, ROLES[0]).0.is_ok()).collect()
    }
}

fn device_path(endpoint_id: &str) -> String {
    format!(r"\\?\SWD#MMDEVAPI#{endpoint_id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
}

pub(crate) fn com_init() -> WinResult<()> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
}

fn err(e: windows::core::Error) -> String {
    e.to_string()
}

/// (id, friendly name) of every active output device.
pub(crate) fn outputs() -> WinResult<Vec<(String, String)>> {
    let mut v = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let coll = enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
        for i in 0..coll.GetCount()? {
            let d = coll.Item(i)?;
            let id = d.GetId()?.to_string()?;
            let pv = d.OpenPropertyStore(STGM_READ)?.GetValue(&PKEY_Device_FriendlyName)?;
            v.push((id, pv.to_string()));
        }
    }
    Ok(v)
}

fn spotify_pids() -> WinResult<Vec<u32>> {
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

pub fn spotify_running() -> Option<bool> {
    spotify_pids().ok().map(|pids| !pids.is_empty())
}

#[derive(Debug)]
pub enum RouteError {
    NoAudioSession,
    Failed(String),
}

impl std::fmt::Display for RouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RouteError::NoAudioSession => write!(f, "Spotify has no audio session yet: start playing something."),
            RouteError::Failed(message) => write!(f, "{message}"),
        }
    }
}

impl From<String> for RouteError {
    fn from(message: String) -> Self {
        RouteError::Failed(message)
    }
}

impl From<&str> for RouteError {
    fn from(message: &str) -> Self {
        RouteError::Failed(message.to_string())
    }
}

pub fn route_spotify_to_cable() -> Result<String, RouteError> {
    com_init().map_err(err)?;

    let (cable_id, cable_name) = outputs()
        .map_err(err)?
        .into_iter()
        .find(|(_, name)| name.to_lowercase().contains("cable input"))
        .ok_or("No virtual cable is installed.")?;
    let pids = spotify_pids().map_err(err)?;
    if pids.is_empty() {
        return Err("Spotify isn't running.".into());
    }
    let policy = Policy::open().map_err(err)?;
    let audio = policy.audio_pids(&pids);
    if audio.is_empty() {
        return Err(RouteError::NoAudioSession);
    }

    let mut routed = 0;
    for &pid in &audio {
        for role in ROLES {
            if policy.set(pid, role, Some(&cable_id)).is_ok() {
                routed += 1;
            }
        }
    }
    if routed == 0 {
        return Err("Windows refused to route Spotify to the cable.".into());
    }
    Ok(format!("routed Spotify to {cable_name}"))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Restore {
    SpotifyNotRunning,
    NoCable,
    NoAudioSession,
    NotRouted,
    Reset(usize),
}

impl std::fmt::Display for Restore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Restore::SpotifyNotRunning => write!(f, "Spotify is not running: nothing to restore"),
            Restore::NoCable => write!(f, "no virtual cable found: nothing to restore"),
            Restore::NoAudioSession => write!(f, "Spotify has no audio session yet: cannot check its output"),
            Restore::NotRouted => write!(f, "Spotify was not routed to the cable"),
            Restore::Reset(n) => write!(f, "reset {n} Spotify routing(s) from the cable back to the default device"),
        }
    }
}

pub fn restore_spotify_if_on_cable() -> Result<Restore, String> {
    com_init().map_err(err)?;

    let pids = spotify_pids().map_err(err)?;
    if pids.is_empty() {
        return Ok(Restore::SpotifyNotRunning);
    }
    let cable_ids: Vec<String> = outputs()
        .map_err(err)?
        .into_iter()
        .filter(|(_, name)| name.to_lowercase().contains("cable input"))
        .map(|(id, _)| id)
        .collect();
    if cable_ids.is_empty() {
        return Ok(Restore::NoCable);
    }

    let policy = Policy::open().map_err(err)?;
    let audio = policy.audio_pids(&pids);
    if audio.is_empty() {
        return Ok(Restore::NoAudioSession);
    }
    let mut reset = 0;
    for &pid in &audio {
        for role in ROLES {
            let (hr, path) = policy.get(pid, role);
            if hr.is_ok() && cable_ids.iter().any(|id| path.contains(id.as_str())) && policy.set(pid, role, None).is_ok() {
                reset += 1;
            }
        }
    }
    Ok(if reset > 0 { Restore::Reset(reset) } else { Restore::NotRouted })
}
