//! Remembered settings: key=value lines in %APPDATA%\NitroRust\settings.txt.
//! Also the "Start with Windows" logon task, which is what makes them apply after a reboot.

use std::collections::BTreeMap;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
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

    pub fn save(&self) {
        let Some(p) = path() else { return };
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let text: String = self.0.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        let _ = std::fs::write(p, text);
    }
}

const TASK: &str = "NitroRust";
/// Logon task name used before the rename; removed so two copies never start.
const OLD_TASK: &str = "NitroSense-rs";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Full path to System32\schtasks.exe. A bare "schtasks" would also be looked up in the
/// exe's own folder first, and this process runs elevated.
fn schtasks_exe() -> std::path::PathBuf {
    use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dir = if n > 0 && n < buf.len() { String::from_utf16_lossy(&buf[..n]) } else { r"C:\Windows\System32".into() };
    std::path::Path::new(&dir).join("schtasks.exe")
}

fn schtasks(args: &[&str]) -> bool {
    Command::new(schtasks_exe())
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_or(false, |o| o.status.success())
}

pub fn autostart_enabled() -> bool {
    schtasks(&["/Query", "/TN", TASK])
}

/// A logon task with highest privileges starts the elevated app without a UAC prompt.
/// Written as XML because schtasks' defaults would stop it on battery and kill it after 72 h.
pub fn set_autostart(on: bool) -> bool {
    if schtasks(&["/Query", "/TN", OLD_TASK]) {
        schtasks(&["/Delete", "/TN", OLD_TASK, "/F"]);
    }
    if !on {
        if !autostart_enabled() {
            return true;
        }
        return schtasks(&["/Delete", "/TN", TASK, "/F"]);
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let user = std::env::var("USERDOMAIN").unwrap_or_default() + "\\" + &std::env::var("USERNAME").unwrap_or_default();
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
    let file = std::env::temp_dir().join("nitrorust-task.xml");
    // UTF-16 with BOM, as the declaration says.
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
    if std::fs::write(&file, bytes).is_err() {
        return false;
    }
    let ok = schtasks(&["/Create", "/TN", TASK, "/XML", &file.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(file);
    ok
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
