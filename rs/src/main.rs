//! NitroSense replacement for Acer Nitro laptops (verified: AN515-45): fans (modes and software curves), power plan,
//! temperatures and keyboard lighting in a Slint window that lives in the tray.
//! The simple view covers everyday use; the "Advanced" switch shows fixed speeds, the curve editor,
//! fan RPM, keyboard effects and per-zone colours.
//! Settings are remembered and re-applied on start; "Start with Windows" brings them back after a reboot.
//! Usage: nitrorust.exe [--tray]        (--tray starts hidden; the logon task uses it)
//!        nitrorust.exe --autostart on|off
//!        nitrorust.exe --exit             (closes a running copy; fans go back to Auto)
#![windows_subsystem = "windows"]

mod acer;
mod caps;
mod curve;
mod power;
mod proto;
mod settings;

use caps::{Caps, Probe};
use curve::Curve;
use settings::Settings;
use slint::{CloseRequestResponse, Color, ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

slint::include_modules!();

/// Hardware writes from sliders and curve dragging go out at most this often.
const THROTTLE: Duration = Duration::from_millis(300);

/// Bright, saturated colours: they look right on the keyboard's LEDs.
const SWATCHES: [[u8; 3]; 10] = [
    [0xFF, 0x00, 0x00], [0xFF, 0x5A, 0x00], [0xFF, 0xD0, 0x00], [0x00, 0xFF, 0x00], [0x00, 0xFF, 0xC8],
    [0x00, 0xA0, 0xFF], [0x00, 0x00, 0xFF], [0x80, 0x00, 0xFF], [0xFF, 0x00, 0xC8], [0xFF, 0xFF, 0xFF],
];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fan {
    Auto,
    /// Index into curve::PRESETS: 0 Quiet, 1 Balanced, 2 Cool.
    Preset(usize),
    Max,
    /// The fixed CPU/GPU speeds from Advanced.
    Fixed,
    /// The user's own curve from Advanced.
    CustomCurve,
}

impl Fan {
    /// The chip the simple view highlights: 0 Auto, 1-3 presets, 4 Max, 5 Custom.
    fn chip(self) -> i32 {
        match self {
            Fan::Auto => 0,
            Fan::Preset(i) => i as i32 + 1,
            Fan::Max => 4,
            Fan::Fixed | Fan::CustomCurve => 5,
        }
    }

    fn from_chip(i: i32) -> Fan {
        match i {
            0 => Fan::Auto,
            1..=3 => Fan::Preset(i as usize - 1),
            _ => Fan::Max,
        }
    }
}

/// Reads the saved fan state. Settings written by the earlier build carry over as they are:
/// fan_mode auto/max/custom/curve, curve_preset 0-2 (a preset) or 3 (the custom curve).
fn load_fan(s: &Settings) -> (Fan, Curve, Curve, bool) {
    let custom = curve::PRESETS.len();
    let preset = s
        .num::<usize>("curve_preset")
        .unwrap_or(if s.get("cpu_curve").is_some() { custom } else { 1 })
        .min(custom);
    let fan = match s.get("fan_mode") {
        Some("max") => Fan::Max,
        Some("custom") => Fan::Fixed,
        Some("curve") if preset < custom => Fan::Preset(preset),
        Some("curve") => Fan::CustomCurve,
        _ => Fan::Auto,
    };
    let default = || Curve::parse(curve::DEFAULT).unwrap();
    let cpu = s.get("cpu_curve").and_then(|t| Curve::parse(t).ok()).unwrap_or_else(default);
    let gpu = s.get("gpu_curve").and_then(|t| Curve::parse(t).ok()).unwrap_or_else(|| cpu.clone());
    let separate = s.get("gpu_separate").map_or(cpu != gpu, |v| v == "1");
    let gpu = if separate { gpu } else { cpu.clone() };
    (fan, cpu, gpu, separate)
}

fn preset_curve(i: usize) -> Curve {
    Curve::parse(curve::PRESETS[i].1).unwrap()
}

/// SVG path in a 100x100 box: x = 30-100 °C, y = fan % (0 at the bottom). Matches Curve::percent:
/// flat before the first point, flat after the last until 90 °C, then 100 %.
fn curve_path(c: &Curve) -> String {
    let x = |t: u32| ((t as f32 - 30.0) / 70.0 * 100.0).clamp(0.0, 100.0);
    let below: Vec<_> = c.points().iter().filter(|(t, _)| *t < curve::FAILSAFE_TEMP).collect();
    let first = c.percent(30); // flat before the first point
    let mut s = format!("M 0 {} ", 100 - first);
    for &&(t, p) in &below {
        s += &format!("L {:.1} {} ", x(t), 100 - p);
    }
    // Just under the failsafe: where the line really ends (it may still be rising toward a point >= 90 °C).
    let edge_t = curve::FAILSAFE_TEMP - 1;
    let edge = c.percent(edge_t);
    if below.last().map_or(true, |p| p.0 < edge_t) {
        s += &format!("L {:.1} {} ", x(edge_t), 100 - edge);
    }
    let fs = x(curve::FAILSAFE_TEMP);
    s += &format!("L {fs:.1} {} L {fs:.1} 0 L 100 0", 100 - edge);
    s
}

type Job = Box<dyn FnOnce(&acer::Acer) + Send>;

enum Msg {
    Job(Job),
    /// Follow these curves (CPU, GPU), or stop following any.
    Curve(Option<(Curve, Curve)>),
}

/// How often the hardware thread reads the sensors (for the window and tray) and runs the curve.
const POLL_PERIOD: Duration = Duration::from_secs(2);

/// The Acer WMI connection, on a thread of its own, which also runs the fan curve.
/// - winit turns the UI thread into a COM single-threaded apartment once its event loop starts
///   (OLE drag and drop), and WMI objects made before that fail with RPC_E_WRONG_THREAD.
/// - The curve (and its 90 °C failsafe) must not stop while the UI thread is busy, e.g. inside
///   the modal colour dialog, which holds up every Slint timer until it closes.
/// - The sensors are read here every POLL_PERIOD whatever the window does (hidden, minimized,
///   in the tray), so the readings shown are never stale and the UI thread never waits on WMI.
/// Messages run in order, so "stop the curve" then "fans auto" can never be undone by a curve step.
struct Hw(std::sync::mpsc::Sender<Msg>);

impl Hw {
    /// Opens the connection and asks the firmware what it supports (read-only).
    /// Also returns the keyboard's current zone colours.
    /// `ui` hears about a curve the firmware keeps refusing (tray action "curve-refused").
    fn open(model: String, pretend_unverified: bool, ui: slint::Weak<AppWindow>) -> Result<(Hw, [[u8; 3]; 4], Caps, Probe), String> {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut a = match acer::Acer::open() {
                Ok(a) => a,
                Err(e) => return drop(ready_tx.send(Err(e.to_string()))),
            };
            let probe = a.probe();
            let caps = Caps::from_probe(&model, &probe, pretend_unverified);
            a.caps = Some(caps.clone());
            let _ = ready_tx.send(Ok((a.zone_colors(), caps, probe)));
            let ui2 = ui.clone();
            let on_telemetry = move |t: acer::Telemetry| {
                let _ = ui2.upgrade_in_event_loop(move |ui| {
                    ui.set_cpu_temp(t.cpu_temp as i32);
                    ui.set_gpu_temp(t.gpu_temp as i32);
                    ui.set_cpu_rpm(t.cpu_rpm as i32);
                    ui.set_gpu_rpm(t.gpu_rpm as i32);
                    ui.invoke_tray_action("telemetry".into());
                });
            };
            hw_loop(&a, rx, on_telemetry, move || {
                let _ = ui.upgrade_in_event_loop(|ui| ui.invoke_tray_action("curve-refused".into()));
            });
        });
        let (zones, caps, probe) = ready_rx.recv().map_err(|e| e.to_string())??;
        Ok((Hw(tx), zones, caps, probe))
    }

    /// Queues a job without waiting for it (for long jobs such as the fan test).
    fn spawn(&self, f: impl FnOnce(&acer::Acer) + Send + 'static) {
        let _ = self.0.send(Msg::Job(Box::new(f)));
    }

    fn run<T: Send + 'static>(&self, f: impl FnOnce(&acer::Acer) -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.0.send(Msg::Job(Box::new(move |a| drop(tx.send(f(a)))))).ok()?;
        rx.recv().ok()
    }

    fn set_curve(&self, curves: Option<(Curve, Curve)>) {
        let _ = self.0.send(Msg::Curve(curves));
    }
}

/// Refused curve writes in a row before the curve is stopped and the fans handed back to Auto.
const CURVE_REFUSALS: u32 = 2;

fn hw_loop(a: &acer::Acer, rx: std::sync::mpsc::Receiver<Msg>, on_telemetry: impl Fn(acer::Telemetry), on_refused: impl Fn()) {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;
    let mut curve: Option<(Curve, Curve)> = None;
    // Speeds the curve last sent (CPU, GPU); 0 = nothing sent yet.
    let mut sent = (0u8, 0u8);
    let mut next = Instant::now();
    let mut refusals = 0;
    loop {
        match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(Msg::Job(job)) => job(a),
            Ok(Msg::Curve(c)) => {
                curve = c;
                sent = (0, 0);
                refusals = 0;
                next = Instant::now(); // apply the new curve at once
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if Instant::now() < next {
            continue;
        }
        next = Instant::now() + POLL_PERIOD;
        let t = a.telemetry();
        on_telemetry(t);
        if let Some((cc, gc)) = curve.as_ref() {
            match run_curve(a, t, cc, gc, sent) {
                Some(s) => {
                    sent = s;
                    refusals = 0;
                }
                None => {
                    // The firmware said no (not a WMI hiccup): stop rather than retry forever.
                    sent = (0, 0);
                    refusals += 1;
                    if refusals >= CURVE_REFUSALS {
                        curve = None;
                        let _ = a.fans_auto();
                        on_refused();
                    }
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Kb {
    on: bool,
    /// true: an animated/static effect in `color`; false: the four zone colours.
    effect_mode: bool,
    effect: u8,
    speed: u8,
    bright: u8,
    left: bool,
    zones: [[u8; 3]; 4],
    color: [u8; 3],
}

struct State {
    hw: Option<Hw>,
    acer_error: String,
    plans: Vec<power::Plan>,
    settings: Settings,
    fan: Fan,
    cpu_curve: Curve,
    gpu_curve: Curve,
    separate: bool,
    editing_gpu: bool,
    fixed: (u8, u8),
    /// A curve point is being dragged (so pointer-up should apply the result).
    dragging: bool,
    kb: Kb,
    visible: bool,
    /// What the laptop supports; None when the Acer controls can't be reached.
    caps: Option<Caps>,
    probe: Probe,
    sys: SysInfo,
    /// Curves and fixed speeds allowed: verified model, or the fan test passed on this model/BIOS.
    fans_unlocked: bool,
    fan_testing: bool,
    /// Message under the fan test button.
    fan_test: String,
    /// What the last fan test measured, for diagnostics.
    fan_test_detail: String,
}

/// Machine details for diagnostics, read from the registry.
#[derive(Default)]
struct SysInfo {
    maker: String,
    model: String,
    bios: String,
    windows: String,
}

impl SysInfo {
    fn read() -> SysInfo {
        let bios = "HARDWARE\\DESCRIPTION\\System\\BIOS";
        let nt = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        let get = |k: &str, v: &str| reg_string(k, v).unwrap_or_default();
        SysInfo {
            maker: get(bios, "SystemManufacturer"),
            model: get(bios, "SystemProductName"),
            bios: get(bios, "BIOSVersion"),
            windows: format!("build {} ({})", get(nt, "CurrentBuild"), get(nt, "DisplayVersion")),
        }
    }
}

impl State {
    /// The curves a mode follows (CPU, GPU), or None for a fixed mode.
    fn curves_for(&self, fan: Fan) -> Option<(Curve, Curve)> {
        match fan {
            Fan::Preset(i) => Some((preset_curve(i), preset_curve(i))),
            Fan::CustomCurve => Some((self.cpu_curve.clone(), self.gpu_curve.clone())),
            _ => None,
        }
    }

    /// The curve the graph shows: the running preset, else the user's curve being edited.
    fn shown_curve(&self) -> Curve {
        match self.fan {
            Fan::Preset(i) => preset_curve(i),
            _ if self.separate && self.editing_gpu => self.gpu_curve.clone(),
            _ => self.cpu_curve.clone(),
        }
    }

    /// Writes an edited curve to the one being edited; without a separate GPU curve, both follow it.
    fn store_edit(&mut self, c: Curve) {
        if let Fan::Preset(i) = self.fan {
            // Editing a preset starts the user's own curve from it. With a separate GPU curve,
            // only the curve being edited is replaced; the user's other curve is kept.
            if !self.separate || self.editing_gpu {
                self.gpu_curve = preset_curve(i);
            }
            if !self.separate || !self.editing_gpu {
                self.cpu_curve = preset_curve(i);
            }
        }
        if self.separate && self.editing_gpu {
            self.gpu_curve = c;
        } else {
            if !self.separate {
                self.gpu_curve = c.clone();
            }
            self.cpu_curve = c;
        }
    }
}

struct App {
    ui: AppWindow,
    tray: Tray,
    st: RefCell<State>,
    fan_timer: Timer,
    kb_timer: Timer,
    /// The graph's points. Updated row by row, never replaced: a new model would recreate
    /// the point being dragged and drop the mouse grab mid-drag.
    points: Rc<VecModel<CurvePoint>>,
    /// The window has been sized to its content once.
    fitted: std::cell::Cell<bool>,
}

fn rgb(c: [u8; 3]) -> Color {
    Color::from_rgb_u8(c[0], c[1], c[2])
}

fn from_color(c: Color) -> [u8; 3] {
    [c.red(), c.green(), c.blue()]
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let v = u32::from_str_radix(s.trim_start_matches('#'), 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

impl App {
    // ---- errors ----

    /// Runs one hardware action. Failures show a plain banner; the raw error goes under "Details".
    fn act(&self, what: &str, f: impl FnOnce(&acer::Acer) -> Result<bool, wmi::WMIError> + Send + 'static) -> bool {
        if self.st.borrow().fan_testing {
            // The hardware thread is busy with the fan test; waiting would freeze the window.
            self.error("Wait for the fan test to finish (about 12 seconds), then try again.", &format!("{what}: fan test running"));
            return false;
        }
        let st = self.st.borrow();
        let result = st.hw.as_ref().map(|h| h.run(move |a| f(a).map_err(|e| e.to_string())).unwrap_or(Err("WMI thread stopped".into())));
        let (text, details) = match result {
            Some(Ok(true)) => {
                drop(st);
                self.clear_error();
                return true;
            }
            None => (
                "Can't reach the laptop's fan and lighting controls. Try running NitroRust as administrator.",
                format!("{what}: Acer WMI unavailable: {}", st.acer_error),
            ),
            Some(Ok(false)) => (
                "The laptop didn't accept that change. Try again, or pick a different setting.",
                format!("{what}: firmware refused"),
            ),
            Some(Err(e)) => ("Something went wrong talking to the laptop. Try again.", format!("{what}: {e}")),
        };
        drop(st);
        self.error(text, &details);
        false
    }

    fn error(&self, text: &str, details: &str) {
        self.ui.set_error_text(text.into());
        self.ui.set_error_details(details.into());
    }

    fn clear_error(&self) {
        self.ui.set_error_text(SharedString::new());
    }

    fn remember(&self, pairs: &[(&str, String)]) {
        let mut st = self.st.borrow_mut();
        for (k, v) in pairs {
            st.settings.set(k, v);
        }
        st.settings.save();
    }

    /// Runs `f` within THROTTLE, once, with whatever the state is by then.
    fn throttle(self: &Rc<Self>, timer: &Timer, f: fn(&Rc<Self>)) {
        if timer.running() {
            return;
        }
        let app = self.clone();
        timer.start(TimerMode::SingleShot, THROTTLE, move || f(&app));
    }

    // ---- window ----

    fn show(self: &Rc<Self>) {
        let _ = self.ui.show();
        if !self.fitted.replace(true) {
            // Size to content on the first showing, once Windows has placed the window.
            let b = self.clone();
            Timer::single_shot(Duration::from_millis(150), move || b.fit_window());
        }
        self.st.borrow_mut().visible = true;
        use slint::winit_030::WinitWindowAccessor;
        self.ui.window().with_winit_window(|w| {
            w.set_minimized(false);
            w.focus_window();
        });
        self.load_plans();
    }

    fn hidden(&self) {
        self.st.borrow_mut().visible = false;
        trim_memory();
    }

    /// Sizes the window to its content, up to the screen's work area, and moves it up
    /// if it would run past the taskbar. The content scrolls when the screen is too short.
    fn fit_window(&self) {
        let w = self.ui.window();
        let scale = w.scale_factor();
        let pos = w.position();
        let (top, bottom) = work_area(pos.x + 50, pos.y + 50);
        let width = w.size().to_logical(scale).width.max(980.0);
        let title_bar = 40.0;
        let room = (bottom - top) as f32 / scale - title_bar;
        let height = self.ui.get_content_height().min(room).max(320.0);
        w.set_size(slint::LogicalSize::new(width, height));
        // position() is the outer top-left (title bar included).
        let needed = ((height + title_bar) * scale) as i32;
        if pos.y + needed > bottom {
            let y = (bottom - needed).max(top);
            w.set_position(slint::PhysicalPosition::new(pos.x, y));
        }
    }

    /// Hands the fans back to the firmware: with the app gone, nothing could undo a
    /// fixed speed or Max, or keep a curve running. The saved mode comes back next start.
    fn exit(&self) {
        self.fan_timer.stop();
        self.kb_timer.stop();
        let st = self.st.borrow();
        if let Some(h) = st.hw.as_ref() {
            h.set_curve(None);
            if st.caps.as_ref().map_or(true, |c| c.fan_control) {
                let _ = h.run(|a| a.fans_auto());
            }
        }
        drop(st);
        let _ = self.tray.hide();
        let _ = slint::quit_event_loop();
    }

    fn tray_action(self: &Rc<Self>, id: &str) {
        match id {
            "open" => self.show(),
            "refit" => self.fit_window(),
            "exit" => self.exit(),
            "curve-refused" => self.curve_refused(),
            "telemetry" => self.update_tray_tooltip(),
            m if m.starts_with("mode") => {
                let fan = Fan::from_chip(m[4..].parse().unwrap_or(0));
                if !self.fan_allowed(fan) {
                    self.show(); // the reason shows in the window
                }
                self.pick_fan(fan);
            }
            _ => {}
        }
    }

    // ---- telemetry ----

    /// The hardware thread has just put fresh readings in the window; mirror them in the tray tooltip.
    fn update_tray_tooltip(&self) {
        let ui = &self.ui;
        let caps = self.st.borrow().caps.clone();
        let Some(c) = caps else { return };
        // Same as the window's tiles: 0 means no reading (e.g. a sleeping dGPU), not 0 °C.
        let temp = |v: i32| if v > 0 { format!("{v} °C") } else { "--".into() };
        let parts: Vec<String> = [
            (c.cpu_temp, format!("CPU {}", temp(ui.get_cpu_temp()))),
            (c.gpu_temp, format!("GPU {}", temp(ui.get_gpu_temp()))),
            (c.cpu_rpm, format!("CPU fan {} rpm", ui.get_cpu_rpm())),
            (c.gpu_rpm, format!("GPU fan {} rpm", ui.get_gpu_rpm())),
        ]
        .into_iter()
        .filter_map(|(has, s)| has.then_some(s))
        .collect();
        let status = if parts.is_empty() { "NitroRust".to_string() } else { format!("NitroRust\n{}", parts.join("\n")) };
        self.tray.set_status(status.into());
    }

    // ---- fans ----

    /// A mode picked by the user (chip or tray): it wins over a slider or drag still waiting to be sent.
    fn pick_fan(&self, fan: Fan) {
        if self.st.borrow().fan_testing {
            return;
        }
        self.fan_timer.stop();
        self.apply_fan(fan);
    }

    // ---- fan test, diagnostics ----

    /// Runs both fans at 30 % then 100 %, checks the RPM follows, then hands them back to Auto.
    /// Runs on the hardware thread; the result arrives through the fan-test-done callback.
    fn start_fan_test(&self) {
        let mut st = self.st.borrow_mut();
        let (Some(h), Some(caps)) = (st.hw.as_ref(), st.caps.clone()) else { return };
        if st.fan_testing || !caps.fan_control {
            return;
        }
        h.set_curve(None);
        let ui = self.ui.as_weak();
        h.spawn(move |a| {
            let (pass, text) = fan_test(a, &caps);
            let _ = ui.upgrade_in_event_loop(move |ui| ui.invoke_fan_test_done(pass, text.into()));
        });
        st.fan_testing = true;
        st.fan = Fan::Auto;
        st.fan_test = "Testing… the fans will speed up for about 12 seconds.".into();
        drop(st);
        self.clear_error();
        self.refresh_fan();
    }

    fn fan_test_done(&self, pass: bool, text: &str) {
        {
            let mut st = self.st.borrow_mut();
            st.fan_testing = false;
            st.fans_unlocked = pass;
            let key = match (&st.caps, pass) {
                (Some(c), true) => c.test_key(&st.sys.bios),
                _ => "fail".into(),
            };
            st.settings.set("fan_test", key);
            st.settings.set("fan_mode", "auto");
            st.settings.save();
            st.fan_test_detail = format!("{} ({text})", if pass { "passed" } else { "failed" });
            st.fan_test = if pass {
                "Fan test passed. All fan modes are unlocked.".into()
            } else {
                format!("Didn't pass ({text}). The fans stay on Auto. Please report this model so it can be added.")
            };
        }
        self.apply_fan(Fan::Auto);
        // The curve panel or a longer message may have appeared.
        let ui = self.ui.as_weak();
        Timer::single_shot(Duration::from_millis(50), move || {
            if let Some(ui) = ui.upgrade() {
                ui.invoke_tray_action("refit".into());
            }
        });
    }

    fn diagnostics(&self) -> String {
        let st = self.st.borrow();
        let now = st
            .hw
            .as_ref()
            .filter(|_| !st.fan_testing)
            .and_then(|h| h.run(|a| a.telemetry()))
            .map_or("no readings".into(), |t| {
                format!("CPU {} °C, GPU {} °C, CPU fan {} rpm, GPU fan {} rpm", t.cpu_temp, t.gpu_temp, t.cpu_rpm, t.gpu_rpm)
            });
        let no_caps = Caps::from_probe(&st.sys.model, &Probe::default(), false);
        let fan_test = if st.fan_test_detail.is_empty() { st.settings.get("fan_test").unwrap_or("not run").to_string() } else { st.fan_test_detail.clone() };
        let fan_test = if st.caps.as_ref().map_or(false, |c| c.verified) { "not needed (verified model)".into() } else { fan_test };
        let text = caps::Report {
            version: env!("CARGO_PKG_VERSION"),
            windows: &st.sys.windows,
            maker: &st.sys.maker,
            bios: &st.sys.bios,
            caps: st.caps.as_ref().unwrap_or(&no_caps),
            probe: &st.probe,
            fan_test: &fan_test,
            now: &now,
        }
        .text();
        if st.caps.is_none() {
            format!("{text}Acer controls unavailable: {}\n", st.acer_error)
        } else {
            text
        }
    }

    fn copy_diagnostics(&self) {
        if set_clipboard(&self.diagnostics()) {
            self.ui.set_diag_note("Diagnostics copied. Paste them into a GitHub issue.".into());
        } else {
            self.error("Couldn't copy to the clipboard. Try again.", "OpenClipboard/SetClipboardData failed");
        }
    }

    /// Opens a pre-filled "model report" issue in the browser; the diagnostics also go to the clipboard.
    fn report_model(&self) {
        let text = self.diagnostics();
        let copied = set_clipboard(&text);
        let model = self.st.borrow().sys.model.clone();
        let url = format!(
            "https://github.com/deadnme/NitroRust/issues/new?template=model-report.yml&title={}&model={}&diagnostics={}",
            caps::url_encode(&format!("Model report: {model}")),
            caps::url_encode(&model),
            caps::url_encode(&text),
        );
        let clip = if copied { " The diagnostics are also on the clipboard." } else { "" };
        if open_url(&url) {
            self.ui.set_diag_note(format!("Opened GitHub in your browser.{clip}").into());
        } else {
            self.error(&format!("Couldn't open the browser.{clip}"), &url);
        }
    }

    /// Auto and Max only need fan control; the rest also needs a verified model or a passed fan test.
    fn fan_allowed(&self, fan: Fan) -> bool {
        let st = self.st.borrow();
        let Some(c) = st.caps.as_ref() else { return true }; // no controls: act() explains
        match fan {
            Fan::Auto | Fan::Max => c.fan_control && !st.fan_testing,
            Fan::Fixed => c.fan_control && st.fans_unlocked && !st.fan_testing,
            // A curve follows the CPU temperature.
            Fan::Preset(_) | Fan::CustomCurve => c.fan_control && c.cpu_temp && st.fans_unlocked && !st.fan_testing,
        }
    }

    fn apply_fan(&self, fan: Fan) {
        if !self.fan_allowed(fan) {
            let (testing, control, cpu_temp, unlocked) = {
                let st = self.st.borrow();
                let c = st.caps.as_ref();
                (st.fan_testing, c.map_or(true, |c| c.fan_control), c.map_or(true, |c| c.cpu_temp), st.fans_unlocked)
            };
            if !testing && fan != Fan::Auto {
                let text = if !control {
                    "This laptop doesn't let apps control its fans."
                } else if !unlocked {
                    "Run the fan control test in Advanced first. It unlocks Quiet, Balanced, Cool and your own settings."
                } else if !cpu_temp {
                    "Fan curves need a CPU temperature, and this laptop doesn't report one."
                } else {
                    "That fan mode isn't available."
                };
                self.error(text, "fan mode blocked by the startup probe or the fan test");
            }
            self.refresh_fan();
            return;
        }
        // Stop a running curve before a fixed mode is sent, so no curve step can override it.
        let previous = {
            let st = self.st.borrow();
            if let (Some(h), None) = (st.hw.as_ref(), st.curves_for(fan)) {
                h.set_curve(None);
            }
            st.curves_for(st.fan)
        };
        let ok = match fan {
            Fan::Auto => self.act("Fans auto", |a| a.fans_auto()),
            Fan::Max => self.act("Fans max", |a| a.fans_max()),
            Fan::Fixed => {
                let (c, g) = self.st.borrow().fixed;
                self.act(&format!("Fans fixed {c}%/{g}%"), move |a| a.fans_custom(c, g))
            }
            // A curve is driven by hw_loop; here we only check the controls are reachable.
            Fan::Preset(_) | Fan::CustomCurve => self.act("Fan curve", |_| Ok(true)),
        };
        if !ok {
            // The new mode was refused: keep the old curve running rather than leave the fans as they are.
            if let (Some(h), Some(c)) = (self.st.borrow().hw.as_ref(), previous) {
                h.set_curve(Some(c));
            }
            let untested = self.st.borrow().caps.as_ref().map_or(false, |c| !c.verified);
            if untested && fan != Fan::Auto {
                self.lock_fans();
            }
        }
        if ok {
            let mut st = self.st.borrow_mut();
            st.fan = fan;
            if let (Some(h), Some(c)) = (st.hw.as_ref(), st.curves_for(fan)) {
                h.set_curve(Some(c));
            }
            let (mode, preset) = match fan {
                Fan::Auto => ("auto", None),
                Fan::Max => ("max", None),
                Fan::Fixed => ("custom", None),
                Fan::Preset(i) => ("curve", Some(i)),
                Fan::CustomCurve => ("curve", Some(curve::PRESETS.len())),
            };
            st.settings.set("fan_mode", mode);
            if let Some(p) = preset {
                st.settings.set("curve_preset", p);
            }
            if fan == Fan::Fixed {
                let (c, g) = st.fixed;
                st.settings.set("cpu_pct", c);
                st.settings.set("gpu_pct", g);
            }
            if fan == Fan::CustomCurve {
                let (c, g) = (st.cpu_curve.text(), st.gpu_curve.text());
                st.settings.set("cpu_curve", c);
                st.settings.set("gpu_curve", g);
            }
            st.settings.save();
        }
        self.refresh_fan();
    }

    /// The hardware thread stopped a curve the firmware kept refusing; the fans are already on Auto.
    fn curve_refused(&self) {
        let untested = self.st.borrow().caps.as_ref().map_or(false, |c| !c.verified);
        if untested {
            return self.lock_fans();
        }
        {
            let mut st = self.st.borrow_mut();
            st.fan = Fan::Auto;
            st.settings.set("fan_mode", "auto");
            st.settings.save();
        }
        self.error("The laptop refused the fan curve, so the fans are back on Auto.", "SetGamingFanSpeed refused twice in a row");
        self.refresh_fan();
    }

    /// An untested model refused a fan write: back to Auto, and curves stay off until a new test passes.
    fn lock_fans(&self) {
        {
            let mut st = self.st.borrow_mut();
            st.fans_unlocked = false;
            st.fan = Fan::Auto;
            st.settings.set("fan_test", "refused");
            st.settings.set("fan_mode", "auto");
            st.settings.save();
            if let Some(h) = st.hw.as_ref() {
                h.set_curve(None);
                let _ = h.run(|a| a.fans_auto());
            }
        }
        self.error(
            "This laptop refused that fan setting, so the fans are back on Auto. Curves are off until the fan test passes again.",
            "fan write refused on an untested model",
        );
        self.refresh_fan();
    }

    fn refresh_fan(&self) {
        let st = self.st.borrow();
        self.ui.set_fans_unlocked(st.fans_unlocked);
        self.ui.set_fan_testing(st.fan_testing);
        self.ui.set_fan_test_text(st.fan_test.as_str().into());
        self.ui.set_fan_mode(st.fan.chip());
        self.tray.set_fan_mode(st.fan.chip());
        let shown = st.shown_curve();
        let pts: Vec<CurvePoint> =
            shown.points().iter().map(|&(t, p)| CurvePoint { t: t as i32, p: p as i32 }).collect();
        use slint::Model;
        if self.points.row_count() == pts.len() {
            for (i, p) in pts.into_iter().enumerate() {
                if self.points.row_data(i).as_ref() != Some(&p) {
                    self.points.set_row_data(i, p);
                }
            }
        } else {
            self.points.set_vec(pts);
        }
        self.ui.set_path_commands(curve_path(&shown).into());
        self.ui.set_curve_text(shown.text().into());
        self.ui.set_curve_error(SharedString::new());
        self.ui.set_separate_gpu(st.separate);
        self.ui.set_curve_target(if st.separate && st.editing_gpu { 1 } else { 0 });
        let caption = match st.fan {
            Fan::Preset(i) => format!("Showing the {} curve. Drag a point to make your own.", curve::PRESETS[i].0),
            Fan::CustomCurve => "Your curve is running. Drag a point or edit the numbers.".into(),
            _ => "Drag a point, or edit the numbers, to run your own curve.".into(),
        };
        self.ui.set_curve_caption(caption.into());
    }

    fn point_dragged(self: &Rc<Self>, i: usize, t: i32, p: i32) {
        let mut st = self.st.borrow_mut();
        let mut pts = st.shown_curve().points().to_vec();
        if i >= pts.len() {
            return;
        }
        let lo = if i > 0 { pts[i - 1].0 + 1 } else { 30 };
        // Points stay below the failsafe, where the graph can show them on the line.
        let hi = if i + 1 < pts.len() { pts[i + 1].0 - 1 } else { curve::FAILSAFE_TEMP - 1 };
        pts[i] = ((t.max(0) as u32).clamp(lo, hi.max(lo)), p.clamp(10, 100) as u8);
        let text: Vec<String> = pts.iter().map(|(t, p)| format!("{t}:{p}")).collect();
        let Ok(c) = Curve::parse(&text.join(" ")) else { return };
        if c == st.shown_curve() {
            return; // mouse jitter on a click: nothing moved
        }
        st.store_edit(c);
        // The curve takes over at once in the UI; the hardware follows within THROTTLE.
        st.fan = Fan::CustomCurve;
        st.dragging = true;
        drop(st);
        self.refresh_fan();
        self.throttle(&self.fan_timer, |a| a.apply_fan(Fan::CustomCurve));
    }

    fn curve_text_entered(&self, text: &str) {
        let parsed = Curve::parse(text);
        let c = match parsed {
            Ok(c) => c,
            Err(e) => return self.ui.set_curve_error(format!("Can't use that: {e}.").into()),
        };
        self.ui.set_curve_error(SharedString::new());
        {
            let mut st = self.st.borrow_mut();
            if c == st.shown_curve() {
                return; // Focus left the box without a change.
            }
            st.store_edit(c);
        }
        self.apply_fan(Fan::CustomCurve);
    }

    fn separate_toggled(&self, on: bool) {
        let custom = {
            let mut st = self.st.borrow_mut();
            st.separate = on;
            if !on {
                st.gpu_curve = st.cpu_curve.clone();
                st.editing_gpu = false;
            }
            st.settings.set("gpu_separate", on as u8);
            st.settings.save();
            st.fan == Fan::CustomCurve
        };
        if custom {
            self.apply_fan(Fan::CustomCurve);
        } else {
            self.refresh_fan();
        }
    }

    fn curve_target_picked(&self, i: i32) {
        self.st.borrow_mut().editing_gpu = i == 1;
        self.refresh_fan();
    }

    fn fixed_changed(self: &Rc<Self>, cpu: i32, gpu: i32) {
        self.st.borrow_mut().fixed = (cpu.clamp(10, 100) as u8, gpu.clamp(10, 100) as u8);
        self.throttle(&self.fan_timer, |a| a.apply_fan(Fan::Fixed));
    }

    // ---- power plan ----

    fn load_plans(&self) {
        let plans = power::plans();
        let active = power::active();
        let names: Vec<SharedString> = plans.iter().map(|p| p.name.as_str().into()).collect();
        let current = plans
            .iter()
            .position(|p| active.as_ref().map_or(false, |g| power::same(g, &p.guid)))
            .map_or(-1, |i| i as i32);
        self.ui.set_power_plans(ModelRc::new(VecModel::from(names)));
        self.ui.set_power_index(current);
        self.st.borrow_mut().plans = plans;
    }

    fn power_picked(&self, i: i32) {
        let st = self.st.borrow();
        let Some(plan) = st.plans.get(i as usize) else { return };
        if power::set_active(&plan.guid) {
            drop(st);
            self.clear_error();
        } else {
            let name = plan.name.clone();
            drop(st);
            self.error("Windows didn't switch the power plan. Try again.", &format!("PowerSetActiveScheme failed for {name}"));
            self.load_plans();
        }
    }

    // ---- keyboard ----

    fn apply_kb(&self) {
        if !self.st.borrow().caps.as_ref().map_or(true, |c| c.kb) {
            return self.refresh_kb();
        }
        let kb = self.st.borrow().kb;
        let bright = if kb.on { kb.bright } else { 0 };
        let ok = if kb.effect_mode {
            self.act(&format!("Keyboard {}", proto::EFFECTS[kb.effect as usize]), move |a| {
                a.keyboard_effect(kb.effect, kb.speed, bright, kb.left, kb.color)
            })
        } else {
            self.act("Keyboard colours", move |a| a.keyboard_zones(bright, &kb.zones))
        };
        if ok {
            self.remember(&[
                ("kb_mode", if kb.effect_mode { "effect" } else { "zones" }.into()),
                ("kb_on", (kb.on as u8).to_string()),
                ("kb_effect", kb.effect.to_string()),
                ("kb_speed", kb.speed.to_string()),
                ("kb_bright", kb.bright.to_string()),
                ("kb_left", (kb.left as u8).to_string()),
                ("kb_color", hex(kb.color)),
                ("zone1", hex(kb.zones[0])),
                ("zone2", hex(kb.zones[1])),
                ("zone3", hex(kb.zones[2])),
                ("zone4", hex(kb.zones[3])),
            ]);
        }
        self.refresh_kb();
    }

    fn refresh_kb(&self) {
        let kb = self.st.borrow().kb;
        let same = kb.zones.iter().all(|z| *z == kb.zones[0]);
        self.ui.set_kb_on(kb.on);
        self.ui.set_kb_brightness(kb.bright as f32);
        self.ui.set_kb_color(rgb(if kb.effect_mode { kb.color } else { kb.zones[0] }));
        self.ui.set_kb_color_known(kb.effect_mode || same);
        let zones_ok = self.st.borrow().caps.as_ref().map_or(true, |c| c.zones);
        // Without zone colours, one colour is the Static effect: no "Effect:" label for it.
        self.ui.set_effect_label(if kb.effect_mode && (zones_ok || kb.effect != 0) {
            format!("Effect: {}", proto::EFFECTS[kb.effect as usize]).into()
        } else {
            SharedString::new()
        });
        self.ui.set_effect_index(kb.effect as i32);
        self.ui.set_kb_speed(kb.speed as f32);
        self.ui.set_kb_left(kb.left);
        let zones: Vec<Color> = kb.zones.iter().map(|z| rgb(*z)).collect();
        self.ui.set_zones(ModelRc::new(VecModel::from(zones)));
    }

    fn kb_update(&self, f: impl FnOnce(&mut Kb)) {
        f(&mut self.st.borrow_mut().kb);
    }

    fn color_picked(&self, c: [u8; 3]) {
        let zones_ok = self.st.borrow().caps.as_ref().map_or(true, |c| c.zones);
        self.kb_update(|kb| {
            kb.zones = [c; 4];
            kb.color = c;
            // Zone layouts differ between models: elsewhere one colour is set as the Static effect.
            kb.effect_mode = !zones_ok;
            if !zones_ok {
                kb.effect = 0;
            }
            kb.on = true;
        });
        self.apply_kb();
    }

    fn zone_picked(&self, i: usize, c: [u8; 3]) {
        if i >= 4 {
            return;
        }
        self.kb_update(|kb| {
            kb.zones[i] = c;
            kb.effect_mode = false;
            kb.on = true;
        });
        self.apply_kb();
    }

    fn initial_color(&self, zone: Option<usize>) -> [u8; 3] {
        let kb = self.st.borrow().kb;
        zone.map_or(kb.color, |i| kb.zones[i.min(3)])
    }
}

/// One curve step: returns the speeds now in force (0 = retry next tick).
/// None = the firmware refused the speeds (it is already back on Auto).
fn run_curve(a: &acer::Acer, t: acer::Telemetry, cc: &Curve, gc: &Curve, (cur_cpu, cur_gpu): (u8, u8)) -> Option<(u8, u8)> {
    if t.cpu_temp == 0 {
        // The CPU sensor read failed: hand control back rather than guess.
        let _ = a.fans_auto();
        return Some((0, 0));
    }
    // A sleeping dGPU reads 0 °C: its fan then follows the curve's lowest point.
    let cpu = curve::next_speed(cur_cpu, cc.percent(t.cpu_temp));
    let gpu = curve::next_speed(cur_gpu, gc.percent(t.gpu_temp));
    if cpu.is_none() && gpu.is_none() {
        return Some((cur_cpu, cur_gpu));
    }
    let (cpu, gpu) = (cpu.unwrap_or(cur_cpu.max(10)), gpu.unwrap_or(cur_gpu.max(10)));
    match a.fans_custom(cpu, gpu) {
        Ok(true) => Some((cpu, gpu)),
        Ok(false) => None,
        Err(_) => Some((0, 0)), // WMI error: retry next tick
    }
}

/// Hands the process's pages back to Windows while it sits in the tray.
fn trim_memory() {
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};
    unsafe { SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Top and bottom of the work area (screen without the taskbar) of the monitor at x, y, in physical pixels.
fn work_area(x: i32, y: i32) -> (i32, i32) {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST};
    let mut mi: MONITORINFO = unsafe { std::mem::zeroed() };
    mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    let ok = unsafe { GetMonitorInfoW(MonitorFromPoint(POINT { x, y }, MONITOR_DEFAULTTONEAREST), &mut mi) };
    if ok != 0 { (mi.rcWork.top, mi.rcWork.bottom) } else { (0, 1000) }
}

/// The Windows accent colour (DWM stores it as 0xAABBGGRR).
fn windows_accent() -> Option<Color> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};
    let (key, value) = (wide("Software\\Microsoft\\Windows\\DWM"), wide("AccentColor"));
    let (mut data, mut size) = (0u32, 4u32);
    let err = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), value.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(),
            &mut data as *mut u32 as *mut _, &mut size)
    };
    (err == 0).then(|| Color::from_rgb_u8(data as u8, (data >> 8) as u8, (data >> 16) as u8))
}

/// The Windows colour dialog. None when cancelled.
fn choose_color(initial: [u8; 3]) -> Option<[u8; 3]> {
    use windows_sys::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    let mut custom = [0x00FF_FFFFu32; 16];
    let mut cc: CHOOSECOLORW = unsafe { std::mem::zeroed() };
    cc.lStructSize = std::mem::size_of::<CHOOSECOLORW>() as u32;
    cc.hwndOwner = unsafe { GetForegroundWindow() };
    cc.rgbResult = initial[0] as u32 | (initial[1] as u32) << 8 | (initial[2] as u32) << 16;
    cc.lpCustColors = custom.as_mut_ptr();
    cc.Flags = CC_FULLOPEN | CC_RGBINIT;
    if unsafe { ChooseColorW(&mut cc) } == 0 {
        return None;
    }
    let v = cc.rgbResult;
    Some([v as u8, (v >> 8) as u8, (v >> 16) as u8])
}

/// The fan test (hardware thread). Returns (passed, what was measured).
fn fan_test(a: &acer::Acer, caps: &Caps) -> (bool, String) {
    let read = || {
        let t = a.telemetry();
        (caps.cpu_rpm.then_some(t.cpu_rpm), caps.gpu_rpm.then_some(t.gpu_rpm))
    };
    let show = |r: (Option<u32>, Option<u32>)| {
        [("CPU", r.0), ("GPU", r.1)]
            .iter()
            .filter_map(|(n, v)| v.map(|v| format!("{n} {v} rpm")))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let hold = |pct: u8| -> Result<(Option<u32>, Option<u32>), String> {
        match a.fans_custom(pct, pct) {
            Ok(true) => {
                std::thread::sleep(Duration::from_secs(6));
                Ok(read())
            }
            Ok(false) => Err("the laptop refused the speed change".into()),
            Err(e) => Err(e.to_string()),
        }
    };
    let result = hold(30).and_then(|low| Ok((low, hold(100)?)));
    let _ = a.fans_auto();
    match result {
        Ok((low, _)) if low.0.is_none() && low.1.is_none() => (false, "no fan speed sensors to check".into()),
        Ok((low, high)) => (caps::fan_test_passed(low, high), format!("30%: {}; 100%: {}", show(low), show(high))),
        Err(e) => (false, e),
    }
}

/// A REG_SZ value under HKEY_LOCAL_MACHINE.
fn reg_string(key: &str, value: &str) -> Option<String> {
    use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
    let (k, v) = (wide(key), wide(value));
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    let err = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr() as *mut _, &mut size)
    };
    if err != 0 {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]).trim().to_string())
}

/// Puts text on the Windows clipboard.
fn set_clipboard(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let data = wide(&text.replace('\n', "\r\n"));
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, data.len() * 2);
        let mut ok = false;
        if !mem.is_null() {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
                GlobalUnlock(mem);
                // On success the clipboard owns the memory.
                ok = !SetClipboardData(CF_UNICODETEXT, mem as _).is_null();
            }
            if !ok {
                GlobalFree(mem);
            }
        }
        CloseClipboard();
        ok
    }
}

/// Opens a web address in the default browser.
fn open_url(url: &str) -> bool {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let (verb, target) = (wide("open"), wide(url));
    let r = unsafe {
        ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), target.as_ptr(), std::ptr::null(), std::ptr::null(), SW_SHOWNORMAL)
    };
    r as isize > 32
}

/// One copy only. Returns true when another copy is already running.
fn already_running() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name = wide(MUTEX);
    unsafe {
        CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        GetLastError() == ERROR_ALREADY_EXISTS
    }
}

const MUTEX: &str = "Local\\nitrorust";
const SHOW_EVENT: &str = "Local\\nitrorust-show";
const EXIT_EVENT: &str = "Local\\nitrorust-exit";

/// Asks the running copy to show its window (SHOW_EVENT) or to exit (EXIT_EVENT).
fn signal_running_copy(event: &str) {
    use windows_sys::Win32::System::Threading::{OpenEventW, SetEvent, EVENT_MODIFY_STATE};
    use windows_sys::Win32::Foundation::CloseHandle;
    let name = wide(event);
    unsafe {
        let h = OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr());
        if !h.is_null() {
            SetEvent(h);
            CloseHandle(h);
        }
    }
}

/// True while a copy holds the single-instance mutex.
fn copy_running() -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{OpenMutexW, SYNCHRONIZATION_SYNCHRONIZE};
    let name = wide(MUTEX);
    unsafe {
        let h = OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, 0, name.as_ptr());
        if h.is_null() {
            return false;
        }
        CloseHandle(h);
        true
    }
}

/// Waits for another copy's signal on `event` and runs the tray action `action` ("open" or "exit").
fn listen_for_second_copy(ui: slint::Weak<AppWindow>, event: &str, action: &'static str) {
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};
    let name = wide(event);
    let h = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if h.is_null() {
        return;
    }
    let h = h as usize; // HANDLE is a raw pointer; the thread only passes it back to Windows.
    std::thread::spawn(move || loop {
        if unsafe { WaitForSingleObject(h as _, INFINITE) } != 0 {
            return;
        }
        let _ = ui.upgrade_in_event_loop(move |ui| ui.invoke_tray_action(action.into()));
    });
}

fn build(ui: AppWindow, tray: Tray) -> Rc<App> {
    let settings = Settings::load();
    let sys = SysInfo::read();
    // For testing safe mode on a verified laptop.
    let pretend = std::env::var("NITRORUST_PRETEND_UNVERIFIED").map_or(false, |v| v == "1");
    let model = if pretend { format!("{} (pretend)", sys.model) } else { sys.model.clone() };
    let (hw, mut zones, acer_error, caps, probe) = match Hw::open(model, pretend, ui.as_weak()) {
        Ok((h, z, c, p)) => (Some(h), z, String::new(), Some(c), p),
        Err(e) => (None, [[255, 255, 255]; 4], e, None, Probe::default()),
    };
    let fans_unlocked = caps.as_ref().map_or(true, |c| {
        c.verified || settings.get("fan_test") == Some(c.test_key(&sys.bios).as_str())
    });
    let (fan, cpu_curve, gpu_curve, separate) = load_fan(&settings);
    let pct = |k: &str| settings.num::<u8>(k).unwrap_or(50).clamp(10, 100);
    for (i, key) in ["zone1", "zone2", "zone3", "zone4"].iter().enumerate() {
        if let Some(c) = settings.get(key).and_then(parse_hex) {
            zones[i] = c;
        }
    }
    let mut kb = Kb {
        on: settings.get("kb_on") != Some("0"),
        effect_mode: settings.get("kb_mode") == Some("effect"),
        effect: settings.num::<u8>("kb_effect").unwrap_or(0).min(proto::EFFECTS.len() as u8 - 1),
        speed: settings.num::<u8>("kb_speed").unwrap_or(5).clamp(1, 9),
        bright: settings.num::<u8>("kb_bright").unwrap_or(100).min(100),
        left: settings.get("kb_left") == Some("1"),
        color: settings.get("kb_color").and_then(parse_hex).unwrap_or(zones[0]),
        zones,
    };
    if caps.as_ref().map_or(false, |c| !c.zones) && !kb.effect_mode {
        // Saved zone colours, but this model gets one colour as the Static effect.
        kb.effect_mode = true;
        kb.effect = 0;
        kb.color = kb.zones[0];
    }
    let st = State {
        hw,
        acer_error,
        plans: Vec::new(),
        fixed: (pct("cpu_pct"), pct("gpu_pct")),
        fan: Fan::Auto,
        cpu_curve,
        gpu_curve,
        separate,
        editing_gpu: false,
        dragging: false,
        kb,
        visible: false,
        settings,
        caps,
        probe,
        sys,
        fans_unlocked,
        fan_testing: false,
        fan_test: String::new(),
        fan_test_detail: String::new(),
    };
    let points = Rc::new(VecModel::default());
    ui.set_points(ModelRc::from(points.clone()));
    Rc::new(App { ui, tray, st: RefCell::new(st), fan_timer: Timer::default(), kb_timer: Timer::default(), points, fitted: Default::default() })
        .init(fan)
}

impl App {
    fn init(self: Rc<Self>, fan: Fan) -> Rc<Self> {
        let ui = &self.ui;
        if let Some(c) = windows_accent() {
            ui.global::<Theme>().set_accent(c);
        }
        let swatches: Vec<Color> = SWATCHES.iter().map(|c| rgb(*c)).collect();
        ui.set_swatches(ModelRc::new(VecModel::from(swatches)));
        let effects: Vec<SharedString> = proto::EFFECTS.iter().map(|e| (*e).into()).collect();
        ui.set_effects(ModelRc::new(VecModel::from(effects)));
        {
            let st = self.st.borrow();
            ui.set_cpu_fixed(st.fixed.0 as f32);
            ui.set_gpu_fixed(st.fixed.1 as f32);
            ui.set_advanced(st.settings.get("advanced") == Some("1"));
        }
        ui.set_autostart(settings::autostart_enabled());
        {
            let st = self.st.borrow();
            if let Some(c) = st.caps.as_ref() {
                ui.set_has_cpu_temp(c.cpu_temp);
                ui.set_has_gpu_temp(c.gpu_temp);
                ui.set_has_fan_control(c.fan_control);
                ui.set_has_gpu_fan(c.gpu_fan);
                ui.set_has_kb(c.kb);
                ui.set_has_zones(c.zones);
                let name = if c.model.is_empty() { "Unknown Acer laptop" } else { c.model.as_str() };
                ui.set_untested_model(if c.verified { SharedString::new() } else { name.into() });
            }
        }
        self.wire();

        // Re-apply what was in force last time, if this laptop allows it.
        let fan = if self.fan_allowed(fan) { fan } else { Fan::Auto };
        if fan != Fan::Auto {
            self.apply_fan(fan);
        } else {
            self.refresh_fan();
        }
        let restore_kb = {
            let st = self.st.borrow();
            st.settings.get("kb_mode").is_some() || !st.kb.on
        };
        if restore_kb {
            self.apply_kb();
        } else {
            self.refresh_kb();
        }

        self
    }

    fn wire(self: &Rc<Self>) {
        let ui = &self.ui;
        macro_rules! on {
            ($setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
                let $app = self.clone();
                ui.$setter(move |$($arg),*| $body);
            }};
        }
        on!(on_fan_mode_picked, |a, i| a.pick_fan(Fan::from_chip(i)));
        on!(on_power_picked, |a, i| a.power_picked(i));
        on!(on_kb_on_toggled, |a, on| {
            a.kb_update(|kb| kb.on = on);
            a.apply_kb();
        });
        on!(on_color_picked, |a, c| a.color_picked(from_color(c)));
        on!(on_more_colors, |a| {
            if let Some(c) = choose_color(a.initial_color(None)) {
                a.color_picked(c);
            }
        });
        on!(on_brightness_changed, |a, v| {
            a.kb_update(|kb| kb.bright = v.clamp(0, 100) as u8);
            a.throttle(&a.kb_timer, |a| a.apply_kb());
        });
        on!(on_autostart_toggled, |a, on| {
            if settings::set_autostart(on) {
                a.clear_error();
            } else {
                a.error("Couldn't change the startup setting. Try again.", "schtasks failed to change the NitroRust logon task");
                a.ui.set_autostart(settings::autostart_enabled());
            }
        });
        on!(on_advanced_toggled, |a, on| {
            a.remember(&[("advanced", (on as u8).to_string())]);
            // Resize once the new section has been laid out.
            let b = a.clone();
            Timer::single_shot(Duration::ZERO, move || b.fit_window());
        });
        on!(on_dismiss_error, |a| a.clear_error());
        on!(on_fixed_changed, |a, c, g| a.fixed_changed(c, g));
        on!(on_point_dragged, |a, i, t, p| a.point_dragged(i.max(0) as usize, t, p));
        on!(on_drag_ended, |a| {
            // Send the final position now rather than waiting for the throttle.
            // A click on a point with no drag changes nothing.
            if std::mem::take(&mut a.st.borrow_mut().dragging) {
                a.fan_timer.stop();
                a.apply_fan(Fan::CustomCurve);
            }
        });
        on!(on_curve_text_entered, |a, t| a.curve_text_entered(&t));
        on!(on_separate_toggled, |a, on| a.separate_toggled(on));
        on!(on_curve_target_picked, |a, i| a.curve_target_picked(i));
        on!(on_effect_picked, |a, i| {
            a.kb_update(|kb| {
                kb.effect = i.clamp(0, proto::EFFECTS.len() as i32 - 1) as u8;
                kb.effect_mode = true;
                kb.on = true;
            });
            a.apply_kb();
        });
        on!(on_kb_speed_changed, |a, v| {
            a.kb_update(|kb| kb.speed = v.clamp(1, 9) as u8);
            if a.st.borrow().kb.effect_mode {
                a.throttle(&a.kb_timer, |a| a.apply_kb());
            }
        });
        on!(on_kb_left_toggled, |a, on| {
            a.kb_update(|kb| kb.left = on);
            if a.st.borrow().kb.effect_mode {
                a.apply_kb();
            }
        });
        on!(on_zone_picked, |a, i, c| a.zone_picked(i.max(0) as usize, from_color(c)));
        on!(on_zone_more, |a, i| {
            let i = i.clamp(0, 3) as usize;
            if let Some(c) = choose_color(a.initial_color(Some(i))) {
                a.zone_picked(i, c);
            }
        });
        on!(on_tray_action, |a, id| a.tray_action(&id));
        on!(on_test_fans, |a| a.start_fan_test());
        on!(on_fan_test_done, |a, pass, text| a.fan_test_done(pass, &text));
        on!(on_copy_diagnostics, |a| a.copy_diagnostics());
        on!(on_report_model, |a| a.report_model());

        let app = self.clone();
        self.tray.on_action(move |id| app.tray_action(&id));
        let app = self.clone();
        ui.window().on_close_requested(move || {
            app.hidden();
            CloseRequestResponse::HideWindow
        });
    }
}

fn main() {
    // nitrorust.exe --autostart on|off : set the logon task without opening the UI.
    let args: Vec<String> = std::env::args().collect();
    // nitrorust.exe --exit : close the running copy the normal way (fans back to Auto). Used by the installer.
    if args.iter().any(|a| a == "--exit") {
        for _ in 0..50 {
            // Signalled every round: a copy that is still starting up has no event yet.
            signal_running_copy(EXIT_EVENT);
            if !copy_running() {
                std::process::exit(0);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        std::process::exit(1);
    }
    if let Some(i) = args.iter().position(|a| a == "--autostart") {
        let ok = settings::set_autostart(args.get(i + 1).map_or(true, |v| v != "off"));
        std::process::exit(if ok { 0 } else { 1 });
    }
    let tray_only = args.iter().any(|a| a == "--tray");
    if already_running() {
        if !tray_only {
            signal_running_copy(SHOW_EVENT);
        }
        return;
    }
    let ui = AppWindow::new().expect("build UI");
    let tray = Tray::new().expect("build tray icon");
    tray.show().expect("show tray icon");
    listen_for_second_copy(ui.as_weak(), SHOW_EVENT, "open");
    listen_for_second_copy(ui.as_weak(), EXIT_EVENT, "exit");
    let app = build(ui, tray);
    if !tray_only {
        app.show();
    } else {
        // Started hidden (logon): hand start-up memory back once settings are applied.
        Timer::single_shot(Duration::from_secs(5), trim_memory);
    }
    slint::run_event_loop_until_quit().expect("event loop");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(pairs: &[(&str, &str)]) -> Settings {
        let mut s = Settings::default();
        for (k, v) in pairs {
            s.set(k, v);
        }
        s
    }

    #[test]
    fn old_settings_carry_over() {
        let bal = "55:20 65:30 75:50 85:80";
        // The file the previous build wrote on this laptop.
        let (fan, cpu, gpu, sep) =
            load_fan(&settings(&[("fan_mode", "auto"), ("curve_preset", "1"), ("cpu_curve", bal), ("gpu_curve", bal)]));
        assert_eq!(fan, Fan::Auto);
        assert_eq!((cpu.text(), gpu.text(), sep), (bal.to_string(), bal.to_string(), false));

        for (i, chip) in [(0, 1), (1, 2), (2, 3)] {
            let (fan, ..) = load_fan(&settings(&[("fan_mode", "curve"), ("curve_preset", &i.to_string())]));
            assert_eq!(fan, Fan::Preset(i));
            assert_eq!(fan.chip(), chip);
        }
        let (fan, ..) = load_fan(&settings(&[("fan_mode", "custom")]));
        assert_eq!((fan, fan.chip()), (Fan::Fixed, 5));
        let (fan, ..) = load_fan(&settings(&[("fan_mode", "max")]));
        assert_eq!(fan.chip(), 4);

        // A custom curve, saved before presets existed: different CPU/GPU curves turn on "Separate GPU curve".
        let (fan, cpu, gpu, sep) =
            load_fan(&settings(&[("fan_mode", "curve"), ("cpu_curve", "50:20 80:80"), ("gpu_curve", "50:30 80:90")]));
        assert_eq!(fan, Fan::CustomCurve);
        assert!(sep);
        assert_eq!((cpu.text().as_str(), gpu.text().as_str()), ("50:20 80:80", "50:30 80:90"));

        // An explicit "not separate" makes the GPU follow the CPU curve.
        let (_, cpu, gpu, sep) = load_fan(&settings(&[("cpu_curve", "50:20 80:80"), ("gpu_curve", "50:30 80:90"), ("gpu_separate", "0")]));
        assert!(!sep);
        assert_eq!(cpu, gpu);
    }

    #[test]
    fn chips_round_trip() {
        for i in 0..5 {
            assert_eq!(Fan::from_chip(i).chip(), i);
        }
    }

    #[test]
    fn path_follows_failsafe() {
        let c = Curve::parse("55:20 65:30 75:50 85:80").unwrap();
        let p = curve_path(&c);
        assert!(p.starts_with("M 0 80 L 35.7 80"), "{p}");
        assert!(p.ends_with("L 85.7 20 L 85.7 0 L 100 0"), "{p}");

        // A point past the failsafe still pulls the line up below 90 °C; the graph must show that.
        let c = Curve::parse("55:20 85:50 99:90").unwrap();
        assert_eq!(c.percent(89), 61);
        let p = curve_path(&c);
        assert!(p.ends_with("L 84.3 39 L 85.7 39 L 85.7 0 L 100 0"), "{p}");
    }

    #[test]
    fn editing_a_preset_keeps_the_other_separate_curve() {
        let mine = Curve::parse("50:30 80:90").unwrap();
        let mut st = State {
            hw: None,
            acer_error: String::new(),
            plans: Vec::new(),
            settings: Settings::default(),
            fan: Fan::Preset(0),
            cpu_curve: mine.clone(),
            gpu_curve: mine.clone(),
            separate: true,
            editing_gpu: true,
            fixed: (50, 50),
            dragging: false,
            kb: Kb { on: true, effect_mode: false, effect: 0, speed: 5, bright: 100, left: false, zones: [[0; 3]; 4], color: [0; 3] },
            visible: false,
            caps: None,
            probe: Probe::default(),
            sys: SysInfo::default(),
            fans_unlocked: true,
            fan_testing: false,
            fan_test: String::new(),
            fan_test_detail: String::new(),
        };
        let edited = Curve::parse("60:20 88:80").unwrap();
        st.store_edit(edited.clone());
        assert_eq!(st.gpu_curve, edited);
        assert_eq!(st.cpu_curve, mine); // not overwritten by the preset
        st.separate = false;
        st.fan = Fan::Preset(1);
        st.store_edit(edited.clone());
        assert_eq!((st.cpu_curve.clone(), st.gpu_curve.clone()), (edited.clone(), edited));
    }
}
