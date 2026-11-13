//! Windows power plans. The AN515-45 firmware has no power profiles of its own
//! (GetGamingMiscSetting(0x0A) bitmap = 0), so NitroSense's "power" control on this model
//! is the active Windows power plan.

use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
use windows_sys::Win32::System::Power::{
    PowerEnumerate, PowerGetActiveScheme, PowerReadFriendlyName, PowerSetActiveScheme,
    ACCESS_SCHEME,
};

pub struct Plan {
    pub guid: GUID,
    pub name: String,
}

pub fn plans() -> Vec<Plan> {
    let mut out = Vec::new();
    for index in 0.. {
        let mut guid: GUID = unsafe { std::mem::zeroed() };
        let mut size = std::mem::size_of::<GUID>() as u32;
        let err = unsafe {
            PowerEnumerate(0 as _, std::ptr::null(), std::ptr::null(), ACCESS_SCHEME, index,
                &mut guid as *mut GUID as *mut u8, &mut size)
        };
        if err != ERROR_SUCCESS {
            break;
        }
        out.push(Plan { name: friendly_name(&guid), guid });
    }
    out
}

fn friendly_name(guid: &GUID) -> String {
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    let err = unsafe {
        PowerReadFriendlyName(0 as _, guid, std::ptr::null(), std::ptr::null(),
            buf.as_mut_ptr() as *mut u8, &mut size)
    };
    if err != ERROR_SUCCESS {
        return "Unnamed plan".into();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

pub fn active() -> Option<GUID> {
    let mut p: *mut GUID = std::ptr::null_mut();
    if unsafe { PowerGetActiveScheme(0 as _, &mut p) } != ERROR_SUCCESS || p.is_null() {
        return None;
    }
    let guid = unsafe { *p };
    unsafe { LocalFree(p as _) };
    Some(guid)
}

pub fn set_active(guid: &GUID) -> bool {
    unsafe { PowerSetActiveScheme(0 as _, guid) == ERROR_SUCCESS }
}

pub fn same(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}
