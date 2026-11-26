//! Remembered settings: key=value lines in %APPDATA%\NitroRust\settings.txt (written atomically).
//! Also the "Start with Windows" logon task, which is what makes them apply after a reboot.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Default)]
pub struct Settings(BTreeMap<String, String>);

fn path_in(dir: &str) -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("APPDATA")?).join(dir).join("settings.txt"))
}

fn path() -> Option<PathBuf> {
    path_in("NitroRust")
}

impl Settings {
    pub fn load() -> Self {
        // Falls back to the folder used before the rename to NitroRust.
        let read = |p: Option<PathBuf>| p.and_then(|p| std::fs::read_to_string(p).ok());
        let text = read(path()).or_else(|| read(path_in("nitrosense-rs"))).unwrap_or_default();
        Self(
            text.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(|s| s.as_str())
    }

    pub fn num<T: std::str::FromStr>(&self, key: &str) -> Option<T> {
        self.get(key)?.parse().ok()
    }

    pub fn set(&mut self, key: &str, value: impl ToString) {
        self.0.insert(key.to_string(), value.to_string());
    }

    /// Saves atomically via a temp file + MoveFileExW so a crash/BSOD never truncates settings.txt.
    pub fn save(&self) {
        let Some(p) = path() else { return };
        let Some(dir) = p.parent() else { return };
        let _ = std::fs::create_dir_all(dir);
        let text: String = self.0.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        let tmp = dir.join("settings.txt.tmp");
        let write_ok = std::fs::File::create(&tmp).and_then(|mut f| {
            f.write_all(text.as_bytes())?;
            f.sync_all()
        });
        if write_ok.is_ok() {
            if !atomic_replace(&tmp, &p) {
                let _ = std::fs::rename(&tmp, &p);
            }
        } else {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn atomic_replace(src: &Path, dst: &Path) -> bool {
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH};
    let (s, d) = (wide(&src.to_string_lossy()), wide(&dst.to_string_lossy()));
    unsafe { MoveFileExW(s.as_ptr(), d.as_ptr(), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) != 0 }
}

const TASK: &str = "NitroRust";
/// Logon task name used before the rename; removed so two copies never start.
const OLD_TASK: &str = "NitroSense-rs";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Full path to System32\schtasks.exe. A bare "schtasks" would also be looked up in the
/// exe's own folder first, and this process runs elevated.
fn schtasks_exe() -> PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dir = if n > 0 && n < buf.len() {
        String::from_utf16_lossy(&buf[..n])
    } else {
        r"C:\Windows\System32".into()
    };
    Path::new(&dir).join("schtasks.exe")
}

fn schtasks_output(args: &[&str]) -> Option<std::process::Output> {
    Command::new(schtasks_exe())
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()
}

fn schtasks(args: &[&str]) -> bool {
    schtasks_output(args).is_some_and(|o| o.status.success())
}

fn task_exists(name: &str) -> bool {
    schtasks(&["/Query", "/TN", name])
}

/// Extracts `<Command>...</Command>` from schtasks' UTF-16 or UTF-8 XML output.
fn extract_xml_command(raw: &[u8]) -> Option<String> {
    let text = if raw.len() >= 2 && raw[0] == 0xFF && raw[1] == 0xFE {
        let u16s: Vec<u16> = raw[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&c| u16::from_le_bytes(c))
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        String::from_utf8_lossy(raw).into_owned()
    };
    let start = text.find("<Command>")? + "<Command>".len();
    let end = text[start..].find("</Command>")?;
    let cmd = text[start..start + end]
        .trim()
        .trim_matches('"')
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    Some(cmd)
}

/// True when the NitroRust logon task exists and points to this executable.
pub fn autostart_enabled() -> bool {
    let Some(out) = schtasks_output(&["/Query", "/TN", TASK, "/XML"]) else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let Ok(exe) = std::env::current_exe() else {
        return true;
    };
    match extract_xml_command(&out.stdout) {
        Some(cmd) => cmd.eq_ignore_ascii_case(&exe.to_string_lossy()),
        None => true,
    }
}

/// Current process user's SID string (S-1-5-...), which Task Scheduler accepts for all
/// account types (Local, Microsoft Account, Domain, Entra ID), even if USERDOMAIN is empty.
fn current_user_id() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut needed = 0u32;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        let mut sid_str = None;
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize];
            if GetTokenInformation(token, TokenUser, buf.as_mut_ptr() as *mut _, needed, &mut needed) != 0 {
                let tu = &*(buf.as_ptr() as *const TOKEN_USER);
                let mut wide_ptr: *mut u16 = std::ptr::null_mut();
                if ConvertSidToStringSidW(tu.User.Sid, &mut wide_ptr) != 0 && !wide_ptr.is_null() {
                    let mut len = 0;
                    while *wide_ptr.add(len) != 0 {
                        len += 1;
                    }
                    sid_str = Some(String::from_utf16_lossy(std::slice::from_raw_parts(wide_ptr, len)));
                    LocalFree(wide_ptr as _);
                }
            }
        }
        CloseHandle(token);
        sid_str
    }
}

/// Creates an exclusive, freshly generated Administrators+SYSTEM-only directory and XML file
/// directly under %ProgramData% (never accepting ERROR_ALREADY_EXISTS or following junctions),
/// returning `(dir, file)` so the caller removes both after `schtasks` reads the XML.
fn write_secure_task_xml(bytes: &[u8]) -> Option<(PathBuf, PathBuf)> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{LocalFree, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL,
    };

    let base = std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));

    // Protected DACL: only BUILTIN\Administrators (BA) and LOCAL SYSTEM (SY) have access.
    let sddl = wide("D:P(A;OICI;FA;;;BA)(A;OICI;FA;;;SY)");
    let mut sd = std::ptr::null_mut();
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) != 0
    };
    if !ok || sd.is_null() {
        return None;
    }

    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };

    // Two OS-CSPRNG RandomState hashes give 128 bits of unpredictable directory name.
    // Requiring CreateDirectoryW != 0 (refusing ERROR_ALREADY_EXISTS) ensures no pre-created
    // folder or junction can ever be reused or re-ACL'd.
    let r1 = RandomState::new().build_hasher().finish();
    let r2 = RandomState::new().build_hasher().finish();
    let dir = base.join(format!("NitroRust-{}-{r1:016x}{r2:016x}", std::process::id()));
    let dir_w = wide(&dir.to_string_lossy());
    let created = unsafe { CreateDirectoryW(dir_w.as_ptr(), &sa) } != 0;

    let result = if created {
        let file = dir.join("task.xml");
        let file_w = wide(&file.to_string_lossy());
        let h = unsafe {
            CreateFileW(
                file_w.as_ptr(),
                GENERIC_WRITE,
                0,
                &sa,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE || h.is_null() {
            let _ = std::fs::remove_dir(&dir);
            None
        } else {
            let mut f = unsafe { std::fs::File::from_raw_handle(h as _) };
            if f.write_all(bytes).is_ok() && f.sync_all().is_ok() {
                drop(f);
                Some((dir, file))
            } else {
                drop(f);
                let _ = std::fs::remove_file(&file);
                let _ = std::fs::remove_dir(&dir);
                None
            }
        }
    } else {
        None
    };

    unsafe { LocalFree(sd) };
    result
}

/// A logon task with highest privileges starts the elevated app without a UAC prompt.
/// Written as XML because schtasks' defaults would stop it on battery and kill it after 72 h.
pub fn set_autostart(on: bool) -> bool {
    if task_exists(OLD_TASK) {
        schtasks(&["/Delete", "/TN", OLD_TASK, "/F"]);
    }
    if !on {
        if !task_exists(TASK) {
            return true;
        }
        return schtasks(&["/Delete", "/TN", TASK, "/F"]);
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let user = current_user_id().or_else(|| {
        let name = std::env::var("USERNAME").ok()?;
        let domain = std::env::var("USERDOMAIN").unwrap_or_default();
        Some(if domain.is_empty() { name } else { format!("{domain}\\{name}") })
    });
    let Some(user) = user else { return false };

    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals><Principal id="Author"><UserId>{user}</UserId><LogonType>InteractiveToken</LogonType><RunLevel>HighestAvailable</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>--tray</Arguments></Exec></Actions>
</Task>"#,
        user = escape(&user),
        exe = escape(&exe.to_string_lossy()),
    );
    // UTF-16 with BOM, as the declaration says.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
    let Some((dir, file)) = write_secure_task_xml(&bytes) else {
        return false;
    };
    let ok = schtasks(&["/Create", "/TN", TASK, "/XML", &file.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&file);
    let _ = std::fs::remove_dir(&dir);
    ok
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_xml_command_utf8_and_utf16() {
        let xml = r#"<Task><Actions><Exec><Command>"C:\Program Files\NitroRust\nitrorust.exe"</Command></Exec></Actions></Task>"#;
        assert_eq!(
            extract_xml_command(xml.as_bytes()).as_deref(),
            Some(r"C:\Program Files\NitroRust\nitrorust.exe")
        );
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
        assert_eq!(
            extract_xml_command(&utf16).as_deref(),
            Some(r"C:\Program Files\NitroRust\nitrorust.exe")
        );
    }

    #[test]
    fn current_user_sid_is_valid() {
        let sid = current_user_id().expect("process token SID");
        assert!(sid.starts_with("S-1-"), "{sid}");
    }
}
