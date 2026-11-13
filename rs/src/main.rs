//! NitroSense replacement for the Acer Nitro AN515-45: fans (modes and software curves), power plan,
//! temperatures and keyboard lighting in a Slint window that lives in the tray.
//! The simple view covers everyday use; the "Advanced" switch shows fixed speeds, the curve editor,
//! fan RPM, keyboard effects and per-zone colours.
//! Settings are remembered and re-applied on start; "Start with Windows" brings them back after a reboot.
//! Usage: nitrorust.exe [--tray]        (--tray starts hidden; the logon task uses it)
//!        nitrorust.exe --autostart on|off
//!        nitrorust.exe --exit             (closes a running copy; fans go back to Auto)
#![windows_subsystem = "windows"]

mod acer;
mod curve;
mod power;
mod proto;
mod settings;

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

/// How often the curve reads the temperatures and adjusts the fans.
const CURVE_PERIOD: Duration = Duration::from_secs(2);

/// The Acer WMI connection, on a thread of its own, which also runs the fan curve.
/// - winit turns the UI thread into a COM single-threaded apartment once its event loop starts
///   (OLE drag and drop), and WMI objects made before that fail with RPC_E_WRONG_THREAD.
/// - The curve (and its 90 °C failsafe) must not stop while the UI thread is busy, e.g. inside
///   the modal colour dialog, which holds up every Slint timer until it closes.
/// Messages run in order, so "stop the curve" then "fans auto" can never be undone by a curve step.
struct Hw(std::sync::mpsc::Sender<Msg>);

impl Hw {
    /// Opens the connection; also returns the keyboard's current zone colours.
    fn open() -> Result<(Hw, [[u8; 3]; 4]), String> {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let a = match acer::Acer::open() {
                Ok(a) => a,
                Err(e) => return drop(ready_tx.send(Err(e.to_string()))),
            };
            let _ = ready_tx.send(Ok(a.zone_colors()));
            hw_loop(&a, rx);
        });
        let zones = ready_rx.recv().map_err(|e| e.to_string())??;
        Ok((Hw(tx), zones))
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

fn hw_loop(a: &acer::Acer, rx: std::sync::mpsc::Receiver<Msg>) {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Instant;
    let mut curve: Option<(Curve, Curve)> = None;
    // Speeds the curve last sent (CPU, GPU); 0 = nothing sent yet.
    let mut sent = (0u8, 0u8);
    let mut next = Instant::now();
    loop {
        let msg = if curve.is_some() {
            rx.recv_timeout(next.saturating_duration_since(Instant::now()))
        } else {
            rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match msg {
            Ok(Msg::Job(job)) => job(a),
            Ok(Msg::Curve(c)) => {
                curve = c;
                sent = (0, 0);
                next = Instant::now(); // apply the new curve at once
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if let Some((cc, gc)) = curve.as_ref() {
            if Instant::now() >= next {
                next = Instant::now() + CURVE_PERIOD;
                sent = run_curve(a, a.temps(), cc, gc, sent);
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
    tick_timer: Timer,
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
        self.tick();
    }

    fn hidden(&self) {
        self.st.borrow_mut().visible = false;
        trim_memory();
    }

    fn on_screen(&self) -> bool {
        self.st.borrow().visible && !self.ui.window().is_minimized()
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
        self.tick_timer.stop();
        self.fan_timer.stop();
        self.kb_timer.stop();
        if let Some(h) = self.st.borrow().hw.as_ref() {
            h.set_curve(None);
            let _ = h.run(|a| a.fans_auto());
        }
        let _ = self.tray.hide();
        let _ = slint::quit_event_loop();
    }

    fn tray_action(self: &Rc<Self>, id: &str) {
        match id {
            "open" => self.show(),
            "exit" => self.exit(),
            m if m.starts_with("mode") => {
                let i = m[4..].parse().unwrap_or(0);
                self.pick_fan(Fan::from_chip(i));
            }
            _ => {}
        }
    }

    // ---- telemetry and the curve loop ----

    /// Reads the sensors for the window, only while it is on screen. The curve runs on the
    /// hardware thread (hw_loop) whatever the window does.
    fn tick(&self) {
        if !self.on_screen() {
            return;
        }
        let st = self.st.borrow();
        let Some(t) = st.hw.as_ref().and_then(|h| h.run(|a| a.telemetry())) else { return };
        drop(st);
        self.ui.set_cpu_temp(t.cpu_temp as i32);
        self.ui.set_gpu_temp(t.gpu_temp as i32);
        self.ui.set_cpu_rpm(t.cpu_rpm as i32);
        self.ui.set_gpu_rpm(t.gpu_rpm as i32);
    }

    // ---- fans ----

    /// A mode picked by the user (chip or tray): it wins over a slider or drag still waiting to be sent.
    fn pick_fan(&self, fan: Fan) {
        self.fan_timer.stop();
        self.apply_fan(fan);
    }

    fn apply_fan(&self, fan: Fan) {
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
        if ok {
            self.tick();
        }
    }

    fn refresh_fan(&self) {
        let st = self.st.borrow();
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
        self.ui.set_effect_label(if kb.effect_mode {
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
        self.kb_update(|kb| {
            kb.zones = [c; 4];
            kb.color = c;
            kb.effect_mode = false;
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
fn run_curve(a: &acer::Acer, t: acer::Telemetry, cc: &Curve, gc: &Curve, (cur_cpu, cur_gpu): (u8, u8)) -> (u8, u8) {
    if t.cpu_temp == 0 {
        // The CPU sensor read failed: hand control back rather than guess.
        let _ = a.fans_auto();
        return (0, 0);
    }
    // A sleeping dGPU reads 0 °C: its fan then follows the curve's lowest point.
    let cpu = curve::next_speed(cur_cpu, cc.percent(t.cpu_temp));
    let gpu = curve::next_speed(cur_gpu, gc.percent(t.gpu_temp));
    if cpu.is_none() && gpu.is_none() {
        return (cur_cpu, cur_gpu);
    }
    let (cpu, gpu) = (cpu.unwrap_or(cur_cpu.max(10)), gpu.unwrap_or(cur_gpu.max(10)));
    if matches!(a.fans_custom(cpu, gpu), Ok(true)) {
        (cpu, gpu)
    } else {
        (0, 0)
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
    let (hw, mut zones, acer_error) = match Hw::open() {
        Ok((h, z)) => (Some(h), z, String::new()),
        Err(e) => (None, [[255, 255, 255]; 4], e),
    };
    let (fan, cpu_curve, gpu_curve, separate) = load_fan(&settings);
    let pct = |k: &str| settings.num::<u8>(k).unwrap_or(50).clamp(10, 100);
    for (i, key) in ["zone1", "zone2", "zone3", "zone4"].iter().enumerate() {
        if let Some(c) = settings.get(key).and_then(parse_hex) {
            zones[i] = c;
        }
    }
    let kb = Kb {
        on: settings.get("kb_on") != Some("0"),
        effect_mode: settings.get("kb_mode") == Some("effect"),
        effect: settings.num::<u8>("kb_effect").unwrap_or(0).min(proto::EFFECTS.len() as u8 - 1),
        speed: settings.num::<u8>("kb_speed").unwrap_or(5).clamp(1, 9),
        bright: settings.num::<u8>("kb_bright").unwrap_or(100).min(100),
        left: settings.get("kb_left") == Some("1"),
        color: settings.get("kb_color").and_then(parse_hex).unwrap_or(zones[0]),
        zones,
    };
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
    };
    let points = Rc::new(VecModel::default());
    ui.set_points(ModelRc::from(points.clone()));
    Rc::new(App { ui, tray, st: RefCell::new(st), tick_timer: Timer::default(), fan_timer: Timer::default(), kb_timer: Timer::default(), points, fitted: Default::default() })
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
        self.wire();

        // Re-apply what was in force last time.
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

        let app = self.clone();
        self.tick_timer.start(TimerMode::Repeated, Duration::from_secs(2), move || app.tick());
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
