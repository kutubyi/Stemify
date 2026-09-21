use std::ffi::c_void;

use windows::core::{Interface, Result, GUID, HRESULT, HSTRING, IUnknown};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::WinRT::RoGetActivationFactory;

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
    _factory: IUnknown, // keeps the object alive
    obj: *mut c_void,
}

impl Policy {
    fn open() -> Result<Self> {
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
}

fn device_path(endpoint_id: &str) -> String {
    format!(r"\\?\SWD#MMDEVAPI#{endpoint_id}#{{e6327cad-dcec-4949-ae8a-991e976a79d2}}")
}

/// (id, friendly name) of every active output device.
fn outputs() -> Result<Vec<(String, String)>> {
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

pub fn spotify_running() -> Option<bool> {
    spotify_pids().ok().map(|pids| !pids.is_empty())
}

pub fn restore_spotify_if_on_cable() -> Result<String> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };

    let pids = spotify_pids()?;
    if pids.is_empty() {
        return Ok("Spotify is not running: nothing to restore".into());
    }
    let cable_ids: Vec<String> =
        outputs()?.into_iter().filter(|(_, name)| name.to_lowercase().contains("cable input")).map(|(id, _)| id).collect();
    if cable_ids.is_empty() {
        return Ok("no virtual cable found: nothing to restore".into());
    }

    let policy = Policy::open()?;
    let mut reset = 0;
    for &pid in &pids {
        for role in ROLES {
            let (hr, path) = policy.get(pid, role);
            if hr.is_ok() && cable_ids.iter().any(|id| path.contains(id.as_str())) && policy.set(pid, role, None).is_ok() {
                reset += 1;
            }
        }
    }
    Ok(if reset > 0 {
        format!("reset {reset} Spotify routing(s) from the cable back to the default device")
    } else {
        "Spotify was not routed to the cable".into()
    })
}
