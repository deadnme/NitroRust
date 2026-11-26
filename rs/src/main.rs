//! NitroSense replacement for Acer Nitro laptops (verified: AN515-45): fans (modes and software curves),
//! firmware thermal profiles, power plan, temperatures and keyboard lighting in a Slint window in the tray.
//! Usage: nitrorust.exe [--tray]        (--tray starts hidden; the logon task uses it)
//!        nitrorust.exe --autostart on|off
//!        nitrorust.exe --exit             (closes a running copy; fans go back to Auto)
#![windows_subsystem = "windows"]

use nitrorust::{app, settings, win32, AppWindow, Tray};
use slint::{ComponentHandle, Timer};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // nitrorust.exe --exit : close the running copy the normal way (fans back to Auto).
    if args.iter().any(|a| a == "--exit") {
        for _ in 0..50 {
            win32::signal_running_copy(win32::EXIT_EVENT);
            if !win32::copy_running() {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::process::exit(1);
    }

    // nitrorust.exe --autostart on|off : set the logon task without opening the UI.
    if let Some(i) = args.iter().position(|a| a == "--autostart") {
        let ok = settings::set_autostart(args.get(i + 1).is_none_or(|v| v != "off"));
        std::process::exit(if ok { 0 } else { 1 });
    }

    let tray_only = args.iter().any(|a| a == "--tray");
    let _instance = match win32::SingleInstance::acquire() {
        Some(guard) => guard,
        None => {
            if !tray_only {
                win32::signal_show_with_retry();
            }
            return;
        }
    };

    let ui = AppWindow::new().expect("build UI");
    let tray = Tray::new().expect("build tray icon");
    tray.show().expect("show tray icon");
    // Top-level hidden listener receives sleep/resume, AC/battery, TaskbarCreated and accent
    // broadcasts even when launched with --tray before AppWindow is ever shown.
    let _sys_listener = win32::SystemListener::start(ui.as_weak());
    win32::listen_for_second_copy(ui.as_weak(), win32::SHOW_EVENT, "open");
    win32::listen_for_second_copy(ui.as_weak(), win32::EXIT_EVENT, "exit");

    let app = app::build(ui, tray);
    if !tray_only {
        app.show();
    } else {
        // Started hidden (logon): hand start-up memory back once settings are applied.
        Timer::single_shot(Duration::from_secs(5), win32::trim_memory);
    }
    slint::run_event_loop_until_quit().expect("event loop");
}
