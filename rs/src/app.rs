//! UI application state, non-blocking WMI orchestration, tray management,
//! sleep/resume & AC/battery policies, and fan curve editing.

use crate::acer::{self, Telemetry};
use crate::caps::{self, Caps, Probe};
use crate::curve::{self, Curve};
use crate::hw::{Hw, HwFanMode};
use crate::power;
use crate::proto;
use crate::settings::{self, Settings};
use crate::win32::{self, SysInfo};
use crate::{AppWindow, CurvePoint, Theme, Tray};
use slint::{CloseRequestResponse, Color, ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

/// Hardware writes from sliders and curve dragging go out at most this often.
const THROTTLE: Duration = Duration::from_millis(300);
/// Settings file saves from rapid slider/curve drags are debounced to avoid disk thrash.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(600);

/// Bright, saturated colours: they look right on the keyboard's LEDs.
const SWATCHES: [[u8; 3]; 10] = [
    [0xFF, 0x00, 0x00], [0xFF, 0x5A, 0x00], [0xFF, 0xD0, 0x00], [0x00, 0xFF, 0x00], [0x00, 0xFF, 0xC8],
    [0x00, 0xA0, 0xFF], [0x00, 0x00, 0xFF], [0x80, 0x00, 0xFF], [0xFF, 0x00, 0xC8], [0xFF, 0xFF, 0xFF],
];

thread_local! {
    /// Weak UI-thread handle so async WMI completions can safely update `App` on the event loop.
    static APP_WEAK: RefCell<Weak<App>> = const { RefCell::new(Weak::new()) };
}

fn with_app(f: impl FnOnce(&Rc<App>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        let app = APP_WEAK.with(|slot| slot.borrow().upgrade());
        if let Some(app) = app {
            f(&app);
        }
    });
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Fan {
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
    pub fn chip(self) -> i32 {
        match self {
            Fan::Auto => 0,
            Fan::Preset(i) => i as i32 + 1,
            Fan::Max => 4,
            Fan::Fixed | Fan::CustomCurve => 5,
        }
    }

    pub fn from_chip(i: i32) -> Fan {
        match i {
            0 => Fan::Auto,
            1..=3 => Fan::Preset(i as usize - 1),
            4 => Fan::Max,
            _ => Fan::CustomCurve,
        }
    }
}

/// Reads the saved fan state. Settings written by earlier builds carry over as they are:
/// fan_mode auto/max/custom/curve, curve_preset 0-2 (a preset) or 3 (the custom curve).
pub fn load_fan(s: &Settings) -> (Fan, Curve, Curve, bool) {
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
pub fn curve_path(c: &Curve) -> String {
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
    if below.last().is_none_or(|p| p.0 < edge_t) {
        s += &format!("L {:.1} {} ", x(edge_t), 100 - edge);
    }
    let fs = x(curve::FAILSAFE_TEMP);
    s += &format!("L {fs:.1} {} L {fs:.1} 0 L 100 0", 100 - edge);
    s
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

pub struct State {
    hw: Option<Hw>,
    acer_error: String,
    plans: Vec<power::Plan>,
    settings: Settings,
    fan: Fan,
    fan_seq: u64,
    kb_seq: u64,
    profile_seq: u64,
    cpu_curve: Curve,
    gpu_curve: Curve,
    separate: bool,
    editing_gpu: bool,
    curve_smooth: bool,
    fixed: (u8, u8),
    /// A curve point is being dragged (so pointer-up should apply and save).
    dragging: bool,
    kb: Kb,
    custom_colors: [u32; 16],
    /// Active firmware thermal profile ID (if laptop exposes GetGamingMiscSetting 0x0A).
    active_profile: Option<u8>,
    /// Automatic Quiet mode on battery.
    quiet_on_battery: bool,
    /// Mode to restore when AC power returns after automatic Quiet switch.
    ac_fan_before_battery: Option<Fan>,
    last_ac: bool,
    visible: bool,
    caps: Option<Caps>,
    probe: Probe,
    sys: SysInfo,
    fans_unlocked: bool,
    fan_testing: bool,
    fan_test: String,
    fan_test_detail: String,
    last_telemetry: Telemetry,
    last_tray_status: String,
    last_accent: Option<Color>,
    last_resume: Option<Instant>,
}

impl State {
    /// The HwFanMode corresponding to a logical Fan mode.
    fn hw_mode_for(&self, fan: Fan) -> HwFanMode {
        match fan {
            Fan::Preset(i) => HwFanMode::Curve {
                cpu: preset_curve(i),
                gpu: preset_curve(i),
                smooth: self.curve_smooth,
            },
            Fan::CustomCurve => HwFanMode::Curve {
                cpu: self.cpu_curve.clone(),
                gpu: self.gpu_curve.clone(),
                smooth: self.curve_smooth,
            },
            Fan::Fixed => HwFanMode::Fixed {
                cpu: self.fixed.0,
                gpu: self.fixed.1,
            },
            Fan::Auto | Fan::Max => HwFanMode::Off,
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
    pub fn store_edit(&mut self, c: Curve) {
        if let Fan::Preset(i) = self.fan {
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

pub struct App {
    pub ui: AppWindow,
    tray: Tray,
    st: RefCell<State>,
    fan_timer: Timer,
    kb_timer: Timer,
    save_timer: Timer,
    /// The graph's points. Updated row by row, never replaced mid-drag.
    points: Rc<VecModel<CurvePoint>>,
    fitted: Cell<bool>,
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

pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let hex = s.trim().strip_prefix('#').unwrap_or(s.trim());
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn parse_custom_colors(s: &str) -> [u32; 16] {
    let mut out = [0x00FF_FFFFu32; 16];
    for (slot, part) in out.iter_mut().zip(s.split(',')) {
        if let Ok(v) = u32::from_str_radix(part.trim(), 16) {
            *slot = v & 0x00FF_FFFF;
        }
    }
    out
}

fn format_custom_colors(colors: &[u32; 16]) -> String {
    colors.iter().map(|c| format!("{:06X}", c & 0x00FF_FFFF)).collect::<Vec<_>>().join(",")
}

/// Initializes keyboard state from saved settings, falling back to the read-only probe bytes
/// so first launch reflects the laptop's real current lighting instead of hardcoded defaults.
fn init_kb(settings: &Settings, probe: &Probe, caps: Option<&Caps>, zones: [[u8; 3]; 4]) -> Kb {
    let probed = probe
        .kb_backlight
        .as_ref()
        .ok()
        .filter(|(ret, _)| *ret == 0)
        .and_then(|(_, bytes)| proto::kb_backlight_from(bytes));

    let (p_mode, p_speed, p_bright, p_left, p_rgb) = probed.unwrap_or((0, 5, 100, false, [0; 3]));
    let default_color = if p_rgb != [0, 0, 0] { p_rgb } else { zones[0] };
    let has_saved = settings.get("kb_mode").is_some();

    let mut kb = Kb {
        on: settings.get("kb_on").map_or(p_bright > 0 || probed.is_none(), |v| v != "0"),
        effect_mode: if has_saved {
            settings.get("kb_mode") == Some("effect")
        } else {
            p_mode != 0
        },
        effect: settings
            .num::<u8>("kb_effect")
            .unwrap_or(p_mode)
            .min(proto::EFFECTS.len() as u8 - 1),
        speed: settings.num::<u8>("kb_speed").unwrap_or(p_speed).clamp(1, 9),
        bright: settings
            .num::<u8>("kb_bright")
            .unwrap_or(if p_bright > 0 { p_bright } else { 100 })
            .min(100),
        left: settings.get("kb_left").map_or(p_left, |v| v == "1"),
        color: settings.get("kb_color").and_then(parse_hex).unwrap_or(default_color),
        zones,
    };
    if caps.is_some_and(|c| !c.zones) && !kb.effect_mode {
        kb.effect_mode = true;
        kb.effect = 0;
        if kb.color == [0, 0, 0] {
            kb.color = [255, 0, 0];
        }
    }
    kb
}

pub fn build(ui: AppWindow, tray: Tray) -> Rc<App> {
    let settings = Settings::load();
    let sys = SysInfo::read();
    let pretend = std::env::var("NITRORUST_PRETEND_UNVERIFIED").is_ok_and(|v| v == "1");
    let model = if pretend { format!("{} (pretend)", sys.model) } else { sys.model.clone() };
    let (hw, mut zones, acer_error, caps, probe, active_profile) = match Hw::open(model, pretend, ui.as_weak()) {
        Ok(init) => (
            Some(init.hw),
            init.zones,
            String::new(),
            Some(init.caps),
            init.probe,
            init.active_profile,
        ),
        Err(e) => (None, [[255, 0, 0]; 4], e, None, Probe::default(), None),
    };
    let fans_unlocked = caps.as_ref().is_none_or(|c| {
        c.verified || settings.get("fan_test") == Some(c.test_key(&sys.bios).as_str())
    });
    let (fan, cpu_curve, gpu_curve, separate) = load_fan(&settings);
    let pct = |k: &str| settings.num::<u8>(k).unwrap_or(50).clamp(10, 100);
    for (i, key) in ["zone1", "zone2", "zone3", "zone4"].iter().enumerate() {
        if let Some(c) = settings.get(key).and_then(parse_hex) {
            zones[i] = c;
        }
    }
    let kb = init_kb(&settings, &probe, caps.as_ref(), zones);
    let custom_colors = settings
        .get("custom_colors")
        .map(parse_custom_colors)
        .unwrap_or([0x00FF_FFFF; 16]);
    let curve_smooth = settings.get("curve_smooth") != Some("0");
    let quiet_on_battery = settings.get("quiet_on_battery") == Some("1");
    let last_ac = win32::is_on_ac_power();
    let accent = win32::windows_accent();

    let st = State {
        hw,
        acer_error,
        plans: Vec::new(),
        fixed: (pct("cpu_pct"), pct("gpu_pct")),
        fan: Fan::Auto,
        fan_seq: 0,
        kb_seq: 0,
        profile_seq: 0,
        cpu_curve,
        gpu_curve,
        separate,
        editing_gpu: false,
        curve_smooth,
        dragging: false,
        kb,
        custom_colors,
        active_profile,
        quiet_on_battery,
        ac_fan_before_battery: None,
        last_ac,
        visible: false,
        settings,
        caps,
        probe,
        sys,
        fans_unlocked,
        fan_testing: false,
        fan_test: String::new(),
        fan_test_detail: String::new(),
        last_telemetry: Telemetry::default(),
        last_tray_status: String::new(),
        last_accent: accent,
        last_resume: None,
    };
    let points = Rc::new(VecModel::default());
    ui.set_points(ModelRc::from(points.clone()));
    let app = Rc::new(App {
        ui,
        tray,
        st: RefCell::new(st),
        fan_timer: Timer::default(),
        kb_timer: Timer::default(),
        save_timer: Timer::default(),
        points,
        fitted: Cell::new(false),
    });
    APP_WEAK.with(|slot| *slot.borrow_mut() = Rc::downgrade(&app));
    app.init(fan)
}

impl App {
    fn init(self: Rc<Self>, mut fan: Fan) -> Rc<Self> {
        let ui = &self.ui;
        if let Some(c) = self.st.borrow().last_accent {
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
            ui.set_curve_smooth(st.curve_smooth);
            ui.set_quiet_on_battery(st.quiet_on_battery);
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
                if !c.profiles.is_empty() {
                    let labels: Vec<SharedString> = c.profiles.iter().map(|(_, n)| (*n).into()).collect();
                    let active_idx = st
                        .active_profile
                        .and_then(|id| c.profiles.iter().position(|&(p, _)| p == id))
                        .map_or(-1, |i| i as i32);
                    ui.set_has_thermal_profiles(true);
                    ui.set_thermal_profiles(ModelRc::new(VecModel::from(labels)));
                    ui.set_thermal_profile_index(active_idx);
                }
            } else {
                // WMI failed to open: disable hardware sections and surface the error immediately.
                ui.set_has_cpu_temp(false);
                ui.set_has_gpu_temp(false);
                ui.set_has_fan_control(false);
                ui.set_has_gpu_fan(false);
                ui.set_has_kb(false);
                ui.set_has_zones(false);
                ui.set_has_thermal_profiles(false);
            }
        }
        self.wire();

        if self.st.borrow().hw.is_none() {
            let err = self.st.borrow().acer_error.clone();
            self.error(
                "Can't reach the laptop's fan and lighting controls. Try running NitroRust as administrator.",
                &format!("Startup WMI open failed: {err}"),
            );
            self.refresh_fan();
            self.refresh_kb();
            return self;
        }

        // Restore saved thermal profile BEFORE fan mode so firmware profile switches cannot
        // reset EC fan behavior back to Auto after we apply our curve/fixed/Max mode.
        let saved_profile = self.st.borrow().settings.num::<u8>("thermal_profile");
        if let Some(id) = saved_profile {
            let idx = self
                .st
                .borrow()
                .caps
                .as_ref()
                .and_then(|c| c.profiles.iter().position(|&(p, _)| p == id));
            if let Some(i) = idx {
                if self.st.borrow().active_profile != Some(id) {
                    self.set_thermal_profile_idx(i as i32, false);
                }
            }
        }

        // Apply saved "Quiet on battery" policy at startup if currently unplugged and Quiet is allowed.
        let allow_quiet = self.fan_allowed(Fan::Preset(0));
        let on_battery_quiet = {
            let mut st = self.st.borrow_mut();
            if allow_quiet && st.quiet_on_battery && !st.last_ac && fan != Fan::Preset(0) {
                st.ac_fan_before_battery = Some(fan);
                true
            } else {
                false
            }
        };
        if on_battery_quiet {
            fan = Fan::Preset(0);
        }

        // Re-apply what was in force last time, if this laptop allows it.
        let fan = if self.fan_allowed(fan) { fan } else { Fan::Auto };
        if fan != Fan::Auto {
            self.apply_fan(fan, false, !on_battery_quiet);
        } else {
            self.refresh_fan();
        }

        let restore_kb = {
            let st = self.st.borrow();
            st.settings.get("kb_mode").is_some() || !st.kb.on
        };
        if restore_kb {
            self.apply_kb(false, false);
        } else {
            self.refresh_kb();
        }

        self
    }

    // ---- errors & persistence ----

    fn error(&self, text: &str, details: &str) {
        self.ui.set_error_text(text.into());
        self.ui.set_error_details(details.into());
    }

    fn clear_error(&self) {
        self.ui.set_error_text(SharedString::new());
    }

    fn remember(&self, pairs: &[(&str, String)], immediate: bool) {
        {
            let mut st = self.st.borrow_mut();
            for (k, v) in pairs {
                st.settings.set(k, v);
            }
            if immediate {
                st.settings.save();
            }
        }
        if immediate {
            self.save_timer.stop();
        } else if !self.save_timer.running() {
            self.save_timer.start(TimerMode::SingleShot, SAVE_DEBOUNCE, || {
                with_app(|a| a.st.borrow().settings.save());
            });
        }
    }

    fn flush_settings(&self) {
        if self.save_timer.running() {
            self.save_timer.stop();
            self.st.borrow().settings.save();
        }
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

    pub fn show(self: &Rc<Self>) {
        let _ = self.ui.show();
        if !self.fitted.replace(true) {
            let b = self.clone();
            Timer::single_shot(Duration::from_millis(150), move || {
                win32::hook_window(&b.ui);
                b.fit_window(true);
            });
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
        self.flush_settings();
        self.st.borrow_mut().visible = false;
        win32::trim_memory();
    }

    /// Sizes the window to its content, up to the screen's work area, and keeps it on screen.
    /// If `widen` narrows a window across the 960px 3-col/2-col breakpoint, schedules a second
    /// height refit once Slint has laid out the stacked two-column content.
    fn fit_window(&self, widen: bool) {
        use slint::winit_030::WinitWindowAccessor;
        let w = self.ui.window();
        if w.is_maximized() {
            return;
        }
        let scale = w.scale_factor();
        let pos = w.position();
        let (left, top, right, bottom) = win32::work_area(pos.x + 50, pos.y + 50);
        let frame = w
            .with_winit_window(|ww| ww.outer_size().width.saturating_sub(ww.inner_size().width) as i32)
            .unwrap_or(0);
        let current = w.size().to_logical(scale).width;
        let max_w = ((right - left - frame) as f32 / scale).max(600.0);
        let width = if widen { current.max(980.0) } else { current }.min(max_w);
        let crossed_narrow = widen && current >= 960.0 && width < 960.0;
        let title_bar = 40.0;
        let room = ((bottom - top) as f32 / scale - title_bar).max(320.0);
        let height = self.ui.get_content_height().min(room).max(320.0);
        w.set_size(slint::LogicalSize::new(width, height));
        let needed = ((height + title_bar) * scale) as i32;
        let y = pos.y.min(bottom - needed).max(top);
        let x = pos.x.min(right - (width * scale) as i32 - frame).max(left);
        if (x, y) != (pos.x, pos.y) {
            w.set_position(slint::PhysicalPosition::new(x, y));
        }
        if crossed_narrow {
            Timer::single_shot(Duration::from_millis(50), || {
                with_app(|a| a.fit_window(false));
            });
        }
    }

    /// Hands the fans back to the firmware and cancels any active fan test so exit is fast.
    fn exit(&self) {
        self.fan_timer.stop();
        self.kb_timer.stop();
        self.flush_settings();
        let st = self.st.borrow();
        if let Some(h) = st.hw.as_ref() {
            h.cancel_fan_test();
            h.set_fan_mode(HwFanMode::Off);
            if st.caps.as_ref().is_none_or(|c| c.fan_control) {
                let _ = h.run_sync(|a| a.fans_auto());
            }
        }
        drop(st);
        let _ = self.tray.hide();
        let _ = slint::quit_event_loop();
    }

    fn tray_action(self: &Rc<Self>, id: &str) {
        match id {
            "open" => self.show(),
            "refit" => self.fit_window(false),
            "exit" => self.exit(),
            "curve-refused" => self.curve_refused(),
            "telemetry" => self.on_telemetry_updated(),
            "power-resume" => self.on_power_resume(),
            "power-status" => self.on_power_status_changed(),
            "taskbar-created" => self.on_taskbar_created(),
            "accent-changed" => self.on_accent_changed(),
            m if m.starts_with("mode") => {
                let fan = Fan::from_chip(m[4..].parse().unwrap_or(0));
                if !self.fan_allowed(fan) {
                    self.show();
                }
                self.pick_fan(fan);
            }
            _ => {}
        }
    }

    // ---- Windows lifecycle events ----

    fn on_taskbar_created(&self) {
        let _ = self.tray.hide();
        let _ = self.tray.show();
        self.st.borrow_mut().last_tray_status.clear();
        self.update_tray_tooltip();
    }

    fn on_accent_changed(&self) {
        if let Some(c) = win32::windows_accent() {
            let mut st = self.st.borrow_mut();
            if st.last_accent != Some(c) {
                st.last_accent = Some(c);
                drop(st);
                self.ui.global::<Theme>().set_accent(c);
            }
        }
    }

    /// Re-applies active hardware settings after Windows wakes from sleep/hibernate.
    fn on_power_resume(&self) {
        let (fan, restore_kb, active_profile) = {
            let mut st = self.st.borrow_mut();
            if st.last_resume.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                return;
            }
            st.last_resume = Some(Instant::now());
            let has_kb = st.caps.as_ref().is_some_and(|c| c.kb);
            let kb_configured = st.settings.get("kb_mode").is_some() || !st.kb.on;
            (st.fan, has_kb && kb_configured, st.active_profile)
        };
        // Re-apply firmware thermal profile FIRST so it cannot reset EC fan behavior afterwards.
        if let Some(id) = active_profile {
            let idx = self
                .st
                .borrow()
                .caps
                .as_ref()
                .and_then(|c| c.profiles.iter().position(|&(p, _)| p == id));
            if let Some(i) = idx {
                self.set_thermal_profile_idx(i as i32, false);
            }
        }
        // Now notify hw_loop to reset cached curve state and re-send Fixed/Max.
        if let Some(h) = self.st.borrow().hw.as_ref() {
            h.notify_resume();
        }
        if matches!(fan, Fan::Fixed | Fan::Max) && self.fan_allowed(fan) {
            self.apply_fan(fan, false, false);
        }
        if restore_kb {
            self.apply_kb(false, false);
        }
        self.on_power_status_changed();
    }

    fn on_power_status_changed(&self) {
        let on_ac = win32::is_on_ac_power();
        let allow_quiet = self.fan_allowed(Fan::Preset(0));
        let target = {
            let mut st = self.st.borrow_mut();
            if on_ac == st.last_ac {
                return;
            }
            st.last_ac = on_ac;
            if !st.quiet_on_battery {
                return;
            }
            if !on_ac {
                if allow_quiet && st.fan != Fan::Preset(0) {
                    st.ac_fan_before_battery = Some(st.fan);
                    Some(Fan::Preset(0))
                } else {
                    None
                }
            } else {
                st.ac_fan_before_battery.take()
            }
        };
        if let Some(fan) = target {
            if self.fan_allowed(fan) {
                self.apply_fan(fan, false, false);
            }
        }
    }

    // ---- telemetry ----

    fn on_telemetry_updated(&self) {
        let t = Telemetry {
            cpu_temp: self.ui.get_cpu_temp().max(0) as u32,
            gpu_temp: self.ui.get_gpu_temp().max(0) as u32,
            cpu_rpm: self.ui.get_cpu_rpm().max(0) as u32,
            gpu_rpm: self.ui.get_gpu_rpm().max(0) as u32,
        };
        self.st.borrow_mut().last_telemetry = t;
        self.update_tray_tooltip();
    }

    fn update_tray_tooltip(&self) {
        let (caps, t) = {
            let st = self.st.borrow();
            (st.caps.clone(), st.last_telemetry)
        };
        let Some(c) = caps else { return };
        let temp = |v: u32| if v > 0 { format!("{v} °C") } else { "--".into() };
        let parts: Vec<String> = [
            (c.cpu_temp, format!("CPU {}", temp(t.cpu_temp))),
            (c.gpu_temp, format!("GPU {}", temp(t.gpu_temp))),
            (c.cpu_rpm, format!("CPU fan {} rpm", t.cpu_rpm)),
            (c.gpu_rpm, format!("GPU fan {} rpm", t.gpu_rpm)),
        ]
        .into_iter()
        .filter_map(|(has, s)| has.then_some(s))
        .collect();
        let status = if parts.is_empty() {
            "NitroRust".to_string()
        } else {
            format!("NitroRust\n{}", parts.join("\n"))
        };
        let changed = {
            let mut st = self.st.borrow_mut();
            if st.last_tray_status == status {
                false
            } else {
                st.last_tray_status = status.clone();
                true
            }
        };
        if changed {
            self.tray.set_status(status.into());
        }
    }

    fn reset_peaks(&self) {
        self.ui.set_cpu_peak(self.ui.get_cpu_temp());
        self.ui.set_gpu_peak(self.ui.get_gpu_temp());
    }

    // ---- fans ----

    fn pick_fan(&self, fan: Fan) {
        if self.st.borrow().fan_testing {
            return;
        }
        // User explicitly picked a mode: clear any pending AC-restore override.
        self.st.borrow_mut().ac_fan_before_battery = None;
        self.fan_timer.stop();
        self.apply_fan(fan, true, true);
    }

    fn start_fan_test(&self) {
        let mut st = self.st.borrow_mut();
        let (Some(h), Some(caps)) = (st.hw.as_ref(), st.caps.clone()) else { return };
        if st.fan_testing || !caps.fan_control {
            return;
        }
        h.reset_fan_test_cancel();
        let cancel = h.cancel_token();
        h.set_fan_mode(HwFanMode::Off);
        let ui = self.ui.as_weak();
        h.spawn(move |a| {
            let (pass, text) = crate::hw::fan_test(a, &caps, &cancel);
            let _ = ui.upgrade_in_event_loop(move |ui| ui.invoke_fan_test_done(pass, text.into()));
        });
        st.fan_testing = true;
        st.fan_seq += 1; // Invalidate any in-flight async apply_fan completions.
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
        self.apply_fan(Fan::Auto, true, true);
        let ui = self.ui.as_weak();
        Timer::single_shot(Duration::from_millis(50), move || {
            if let Some(ui) = ui.upgrade() {
                ui.invoke_tray_action("refit".into());
            }
        });
    }

    fn diagnostics(&self) -> String {
        let st = self.st.borrow();
        let t = st.last_telemetry;
        let now = if st.hw.is_some() && (t.cpu_temp > 0 || t.gpu_temp > 0 || t.cpu_rpm > 0 || t.gpu_rpm > 0) {
            format!(
                "CPU {} °C, GPU {} °C, CPU fan {} rpm, GPU fan {} rpm",
                t.cpu_temp, t.gpu_temp, t.cpu_rpm, t.gpu_rpm
            )
        } else {
            "no readings".into()
        };
        let no_caps = Caps::from_probe(&st.sys.model, &Probe::default(), false);
        let fan_test = if st.fan_test_detail.is_empty() {
            st.settings.get("fan_test").unwrap_or("not run").to_string()
        } else {
            st.fan_test_detail.clone()
        };
        let fan_test = if st.caps.as_ref().is_some_and(|c| c.verified) {
            "not needed (verified model)".into()
        } else {
            fan_test
        };
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
        if win32::set_clipboard(&self.diagnostics()) {
            self.ui.set_diag_note("Diagnostics copied. Paste them into a GitHub issue.".into());
        } else {
            self.error("Couldn't copy to the clipboard. Try again.", "OpenClipboard/SetClipboardData failed");
        }
    }

    fn report_model(&self) {
        let text = self.diagnostics();
        let copied = win32::set_clipboard(&text);
        let model = self.st.borrow().sys.model.clone();
        let url = format!(
            "https://github.com/deadnme/NitroRust/issues/new?template=model-report.yml&title={}&model={}&diagnostics={}",
            caps::url_encode(&format!("Model report: {model}")),
            caps::url_encode(&model),
            caps::url_encode(&text),
        );
        let clip = if copied { " The diagnostics are also on the clipboard." } else { "" };
        if win32::open_url(&url) {
            self.ui.set_diag_note(format!("Opened GitHub in your browser.{clip}").into());
        } else {
            self.error(&format!("Couldn't open the browser.{clip}"), &url);
        }
    }

    fn fan_allowed(&self, fan: Fan) -> bool {
        let st = self.st.borrow();
        let Some(c) = st.caps.as_ref() else { return false };
        match fan {
            Fan::Auto | Fan::Max => c.fan_control && !st.fan_testing,
            Fan::Fixed => c.fan_control && st.fans_unlocked && !st.fan_testing,
            Fan::Preset(_) | Fan::CustomCurve => c.fan_control && c.cpu_temp && st.fans_unlocked && !st.fan_testing,
        }
    }

    /// Applies `fan` asynchronously on the hardware thread without blocking the UI.
    fn apply_fan(&self, fan: Fan, save_now: bool, persist: bool) {
        if !self.fan_allowed(fan) {
            let (testing, has_hw, control, cpu_temp, unlocked, acer_err) = {
                let st = self.st.borrow();
                let c = st.caps.as_ref();
                (
                    st.fan_testing,
                    st.hw.is_some(),
                    c.is_some_and(|c| c.fan_control),
                    c.is_some_and(|c| c.cpu_temp),
                    st.fans_unlocked,
                    st.acer_error.clone(),
                )
            };
            if !testing && fan != Fan::Auto {
                if !has_hw {
                    self.error(
                        "Can't reach the laptop's fan and lighting controls. Try running NitroRust as administrator.",
                        &format!("Acer WMI unavailable: {acer_err}"),
                    );
                } else {
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
            }
            self.refresh_fan();
            return;
        }

        let (prev_fan, prev_hw_mode, next_hw_mode, seq, fixed) = {
            let mut st = self.st.borrow_mut();
            let prev_fan = st.fan;
            let prev_hw_mode = st.hw_mode_for(prev_fan);
            st.fan = fan;
            st.fan_seq += 1;
            let seq = st.fan_seq;
            let next_hw_mode = st.hw_mode_for(fan);
            let fixed = st.fixed;
            (prev_fan, prev_hw_mode, next_hw_mode, seq, fixed)
        };

        // Curve modes are driven directly by hw_loop (no synchronous write needed here).
        if matches!(fan, Fan::Preset(_) | Fan::CustomCurve) {
            if let Some(h) = self.st.borrow().hw.as_ref() {
                h.set_fan_mode(next_hw_mode);
            }
            self.clear_error();
            if persist {
                self.persist_fan(fan, save_now);
            }
            self.refresh_fan();
            return;
        }

        // Fixed / Auto / Max: stop any previous curve first, run WMI write asynchronously.
        let what = match fan {
            Fan::Auto => "Fans auto".to_string(),
            Fan::Max => "Fans max".to_string(),
            Fan::Fixed => format!("Fans fixed {}%/{}%", fixed.0, fixed.1),
            _ => unreachable!(),
        };

        let spawned = {
            let st = self.st.borrow();
            if let Some(h) = st.hw.as_ref() {
                h.set_fan_mode(HwFanMode::Off);
                let next_for_cb = next_hw_mode.clone();
                let prev_for_cb = prev_hw_mode.clone();
                h.spawn(move |a| {
                    let res = match fan {
                        Fan::Auto => a.fans_auto(),
                        Fan::Max => a.fans_max(),
                        Fan::Fixed => a.fans_custom(fixed.0, fixed.1),
                        _ => Ok(true),
                    };
                    with_app(move |app| {
                        app.on_fan_write_done(
                            seq,
                            fan,
                            prev_fan,
                            prev_for_cb,
                            next_for_cb,
                            what,
                            res,
                            save_now,
                            persist,
                        );
                    });
                });
                true
            } else {
                false
            }
        };

        if !spawned {
            let err = self.st.borrow().acer_error.clone();
            self.st.borrow_mut().fan = prev_fan;
            self.error(
                "Can't reach the laptop's fan and lighting controls. Try running NitroRust as administrator.",
                &format!("Acer WMI unavailable: {err}"),
            );
        }
        self.refresh_fan();
    }

    #[allow(clippy::too_many_arguments)]
    fn on_fan_write_done(
        &self,
        seq: u64,
        fan: Fan,
        prev_fan: Fan,
        prev_hw_mode: HwFanMode,
        next_hw_mode: HwFanMode,
        what: String,
        res: acer::R<bool>,
        save_now: bool,
        persist: bool,
    ) {
        if self.st.borrow().fan_seq != seq {
            return; // Superseded by a newer user action.
        }
        match res {
            Ok(true) => {
                if matches!(fan, Fan::Fixed) {
                    if let Some(h) = self.st.borrow().hw.as_ref() {
                        h.set_fan_mode(next_hw_mode);
                    }
                }
                self.clear_error();
                if persist {
                    self.persist_fan(fan, save_now);
                }
                self.refresh_fan();
            }
            Ok(false) => {
                // Genuine firmware refusal. Only lock untested models when Fixed speed is refused.
                let untested = self.st.borrow().caps.as_ref().is_some_and(|c| !c.verified);
                if untested && fan == Fan::Fixed {
                    self.lock_fans();
                    return;
                }
                {
                    let mut st = self.st.borrow_mut();
                    st.fan = prev_fan;
                    if let Some(h) = st.hw.as_ref() {
                        h.set_fan_mode(prev_hw_mode);
                    }
                }
                self.error(
                    "The laptop didn't accept that change. Try again, or pick a different setting.",
                    &format!("{what}: firmware refused"),
                );
                self.refresh_fan();
            }
            Err(e) => {
                // WMI/transport error: revert to previous mode WITHOUT revoking untested fan unlock.
                {
                    let mut st = self.st.borrow_mut();
                    st.fan = prev_fan;
                    if let Some(h) = st.hw.as_ref() {
                        h.set_fan_mode(prev_hw_mode);
                    }
                }
                self.error("Something went wrong talking to the laptop. Try again.", &format!("{what}: {e}"));
                self.refresh_fan();
            }
        }
    }

    fn persist_fan(&self, fan: Fan, save_now: bool) {
        let mut pairs: Vec<(&str, String)> = Vec::new();
        let (mode, preset) = match fan {
            Fan::Auto => ("auto", None),
            Fan::Max => ("max", None),
            Fan::Fixed => ("custom", None),
            Fan::Preset(i) => ("curve", Some(i)),
            Fan::CustomCurve => ("curve", Some(curve::PRESETS.len())),
        };
        pairs.push(("fan_mode", mode.into()));
        if let Some(p) = preset {
            pairs.push(("curve_preset", p.to_string()));
        }
        {
            let st = self.st.borrow();
            if fan == Fan::Fixed {
                pairs.push(("cpu_pct", st.fixed.0.to_string()));
                pairs.push(("gpu_pct", st.fixed.1.to_string()));
            }
            if fan == Fan::CustomCurve {
                pairs.push(("cpu_curve", st.cpu_curve.text()));
                pairs.push(("gpu_curve", st.gpu_curve.text()));
            }
        }
        self.remember(&pairs, save_now);
    }

    fn curve_refused(&self) {
        let untested = self.st.borrow().caps.as_ref().is_some_and(|c| !c.verified);
        if untested {
            return self.lock_fans();
        }
        {
            let mut st = self.st.borrow_mut();
            st.fan_seq += 1;
            st.fan = Fan::Auto;
            st.settings.set("fan_mode", "auto");
            st.settings.save();
        }
        self.error(
            "The laptop refused the fan curve, so the fans are back on Auto.",
            "SetGamingFanSpeed refused twice in a row",
        );
        self.refresh_fan();
    }

    fn lock_fans(&self) {
        {
            let mut st = self.st.borrow_mut();
            st.fan_seq += 1;
            st.fans_unlocked = false;
            st.fan = Fan::Auto;
            st.settings.set("fan_test", "refused");
            st.settings.set("fan_mode", "auto");
            st.settings.save();
            if let Some(h) = st.hw.as_ref() {
                h.set_fan_mode(HwFanMode::Off);
                h.spawn(|a| {
                    let _ = a.fans_auto();
                });
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
        let pts: Vec<CurvePoint> = shown.points().iter().map(|&(t, p)| CurvePoint { t: t as i32, p: p as i32 }).collect();
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
        self.ui.set_curve_smooth(st.curve_smooth);
        let caption = match st.fan {
            Fan::Preset(i) => format!("Showing the {} curve. Drag a point to make your own.", curve::PRESETS[i].0),
            Fan::CustomCurve => "Your curve is running. Drag a point or edit the numbers.".into(),
            _ => "Drag a point, or edit the numbers, to run your own curve.".into(),
        };
        self.ui.set_curve_caption(caption.into());
    }

    fn point_dragged(self: &Rc<Self>, i: usize, t: i32, p: i32) {
        // Validate permission BEFORE mutating st.fan so disallowed drags never leave stale UI state.
        if !self.fan_allowed(Fan::CustomCurve) {
            return;
        }
        let mut st = self.st.borrow_mut();
        let mut pts = st.shown_curve().points().to_vec();
        if i >= pts.len() {
            return;
        }
        let lo = if i > 0 { pts[i - 1].0 + 1 } else { 30 };
        let hi = if i + 1 < pts.len() {
            pts[i + 1].0.saturating_sub(1).min(curve::FAILSAFE_TEMP - 1)
        } else {
            curve::FAILSAFE_TEMP - 1
        };
        pts[i] = ((t.max(0) as u32).clamp(lo, hi.max(lo)), p.clamp(10, 100) as u8);
        let text: Vec<String> = pts.iter().map(|(t, p)| format!("{t}:{p}")).collect();
        let Ok(c) = Curve::parse(&text.join(" ")) else { return };
        if c == st.shown_curve() {
            return;
        }
        st.store_edit(c);
        st.ac_fan_before_battery = None;
        st.fan = Fan::CustomCurve;
        st.dragging = true;
        drop(st);
        self.refresh_fan();
        self.throttle(&self.fan_timer, |a| a.apply_fan(Fan::CustomCurve, false, true));
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
                return;
            }
            st.store_edit(c);
            st.ac_fan_before_battery = None;
        }
        self.apply_fan(Fan::CustomCurve, true, true);
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
            self.apply_fan(Fan::CustomCurve, true, true);
        } else {
            self.refresh_fan();
        }
    }

    fn curve_smooth_toggled(&self, on: bool) {
        let fan = {
            let mut st = self.st.borrow_mut();
            st.curve_smooth = on;
            st.settings.set("curve_smooth", on as u8);
            st.settings.save();
            st.fan
        };
        if matches!(fan, Fan::Preset(_) | Fan::CustomCurve) {
            if let Some(h) = self.st.borrow().hw.as_ref() {
                let mode = self.st.borrow().hw_mode_for(fan);
                h.set_fan_mode(mode);
            }
        }
    }

    fn quiet_on_battery_toggled(&self, on: bool) {
        let on_ac = win32::is_on_ac_power();
        let allow_quiet = self.fan_allowed(Fan::Preset(0));
        let action = {
            let mut st = self.st.borrow_mut();
            st.last_ac = on_ac;
            st.quiet_on_battery = on;
            st.settings.set("quiet_on_battery", on as u8);
            st.settings.save();
            if on && !on_ac && allow_quiet && st.fan != Fan::Preset(0) {
                st.ac_fan_before_battery = Some(st.fan);
                Some(Fan::Preset(0))
            } else if !on {
                st.ac_fan_before_battery.take()
            } else {
                None
            }
        };
        if let Some(fan) = action {
            if self.fan_allowed(fan) {
                self.apply_fan(fan, false, false);
            }
        }
    }

    fn curve_target_picked(&self, i: i32) {
        self.st.borrow_mut().editing_gpu = i == 1;
        self.refresh_fan();
    }

    fn fixed_changed(self: &Rc<Self>, cpu: i32, gpu: i32) {
        {
            let mut st = self.st.borrow_mut();
            st.fixed = (cpu.clamp(10, 100) as u8, gpu.clamp(10, 100) as u8);
            st.ac_fan_before_battery = None;
        }
        self.throttle(&self.fan_timer, |a| a.apply_fan(Fan::Fixed, false, true));
    }

    // ---- firmware thermal profile ----

    fn thermal_profile_picked(&self, i: i32) {
        self.set_thermal_profile_idx(i, true);
    }

    fn set_thermal_profile_idx(&self, i: i32, persist: bool) {
        if i < 0 {
            return;
        }
        let (prev_id, target, seq) = {
            let mut st = self.st.borrow_mut();
            let Some(&(id, name)) = st.caps.as_ref().and_then(|c| c.profiles.get(i as usize)) else {
                return;
            };
            let prev = st.active_profile;
            st.active_profile = Some(id);
            st.profile_seq += 1;
            (prev, (id, name), st.profile_seq)
        };
        self.sync_profile_index();
        let spawned = {
            let st = self.st.borrow();
            if let Some(h) = st.hw.as_ref() {
                h.spawn(move |a| {
                    let res = a.set_thermal_profile(target.0);
                    with_app(move |app| app.on_profile_done(seq, target.0, target.1, prev_id, res, persist));
                });
                true
            } else {
                false
            }
        };
        if !spawned {
            self.st.borrow_mut().active_profile = prev_id;
            self.sync_profile_index();
        }
    }

    fn on_profile_done(&self, seq: u64, id: u8, name: &str, prev_id: Option<u8>, res: acer::R<bool>, persist: bool) {
        if self.st.borrow().profile_seq != seq {
            return;
        }
        match res {
            Ok(true) => {
                self.clear_error();
                if persist {
                    self.remember(&[("thermal_profile", id.to_string())], true);
                }
            }
            Ok(false) => {
                self.st.borrow_mut().active_profile = prev_id;
                self.sync_profile_index();
                self.error(
                    "The laptop didn't accept that thermal profile.",
                    &format!("SetGamingMiscSetting({name}) refused"),
                );
            }
            Err(e) => {
                self.st.borrow_mut().active_profile = prev_id;
                self.sync_profile_index();
                self.error(
                    "Something went wrong switching the thermal profile.",
                    &format!("SetGamingMiscSetting({name}): {e}"),
                );
            }
        }
    }

    fn sync_profile_index(&self) {
        let idx = {
            let st = self.st.borrow();
            st.active_profile
                .and_then(|id| st.caps.as_ref()?.profiles.iter().position(|&(p, _)| p == id))
                .map_or(-1, |i| i as i32)
        };
        self.ui.set_thermal_profile_index(idx);
    }

    // ---- power plan ----

    fn load_plans(&self) {
        let plans = power::plans();
        let active = power::active();
        let names: Vec<SharedString> = plans.iter().map(|p| p.name.as_str().into()).collect();
        let current = plans
            .iter()
            .position(|p| active.as_ref().is_some_and(|g| power::same(g, &p.guid)))
            .map_or(-1, |i| i as i32);
        self.ui.set_power_plans(ModelRc::new(VecModel::from(names)));
        self.ui.set_power_index(current);
        self.st.borrow_mut().plans = plans;
    }

    fn power_picked(&self, i: i32) {
        if i < 0 {
            return;
        }
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

    fn apply_kb(&self, save_now: bool, persist: bool) {
        if !self.st.borrow().caps.as_ref().is_none_or(|c| c.kb) {
            return self.refresh_kb();
        }
        let (kb, seq) = {
            let mut st = self.st.borrow_mut();
            st.kb_seq += 1;
            (st.kb, st.kb_seq)
        };
        let bright = if kb.on { kb.bright } else { 0 };
        let what = if kb.effect_mode {
            format!("Keyboard {}", proto::EFFECTS[kb.effect as usize])
        } else {
            "Keyboard colours".into()
        };

        let spawned = {
            let st = self.st.borrow();
            if st.fan_testing {
                drop(st);
                self.error(
                    "Wait for the fan test to finish (about 12 seconds), then try again.",
                    &format!("{what}: fan test running"),
                );
                return self.refresh_kb();
            }
            if let Some(h) = st.hw.as_ref() {
                let what_cb = what.clone();
                h.spawn(move |a| {
                    let res = if kb.effect_mode {
                        a.keyboard_effect(kb.effect, kb.speed, bright, kb.left, kb.color)
                    } else {
                        a.keyboard_zones(bright, &kb.zones)
                    };
                    with_app(move |app| app.on_kb_write_done(seq, kb, what_cb, res, save_now, persist));
                });
                true
            } else {
                false
            }
        };

        if !spawned {
            let err = self.st.borrow().acer_error.clone();
            self.error(
                "Can't reach the laptop's fan and lighting controls. Try running NitroRust as administrator.",
                &format!("{what}: Acer WMI unavailable: {err}"),
            );
        }
        self.refresh_kb();
    }

    fn on_kb_write_done(&self, seq: u64, kb: Kb, what: String, res: acer::R<bool>, save_now: bool, persist: bool) {
        if self.st.borrow().kb_seq != seq {
            return;
        }
        match res {
            Ok(true) => {
                self.clear_error();
                if persist {
                    self.remember(
                        &[
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
                        ],
                        save_now,
                    );
                }
            }
            Ok(false) => {
                self.error(
                    "The laptop didn't accept that change. Try again, or pick a different setting.",
                    &format!("{what}: firmware refused"),
                );
            }
            Err(e) => {
                self.error("Something went wrong talking to the laptop. Try again.", &format!("{what}: {e}"));
            }
        }
    }

    fn refresh_kb(&self) {
        let kb = self.st.borrow().kb;
        let same = kb.zones.iter().all(|z| *z == kb.zones[0]);
        self.ui.set_kb_on(kb.on);
        self.ui.set_kb_brightness(kb.bright as f32);
        self.ui.set_kb_color(rgb(if kb.effect_mode { kb.color } else { kb.zones[0] }));
        self.ui.set_kb_color_known(kb.effect_mode || same);
        let zones_ok = self.st.borrow().caps.as_ref().is_none_or(|c| c.zones);
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
        let zones_ok = self.st.borrow().caps.as_ref().is_none_or(|c| c.zones);
        self.kb_update(|kb| {
            kb.zones = [c; 4];
            kb.color = c;
            kb.effect_mode = !zones_ok;
            if !zones_ok {
                kb.effect = 0;
            }
            kb.on = true;
        });
        self.apply_kb(true, true);
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
        self.apply_kb(true, true);
    }

    fn pick_custom_color(&self, zone: Option<usize>) -> Option<[u8; 3]> {
        let (initial, mut custom) = {
            let st = self.st.borrow();
            let init = zone.map_or(st.kb.color, |i| st.kb.zones[i.min(3)]);
            (init, st.custom_colors)
        };
        let owner = win32::hwnd_of(&self.ui);
        let picked = win32::choose_color(owner, initial, &mut custom);
        let changed = {
            let mut st = self.st.borrow_mut();
            let diff = st.custom_colors != custom;
            st.custom_colors = custom;
            diff
        };
        if changed {
            self.remember(&[("custom_colors", format_custom_colors(&custom))], true);
        }
        picked
    }

    fn wire(self: &Rc<Self>) {
        let ui = &self.ui;
        macro_rules! on {
            ($setter:ident, |$app:ident $(, $arg:ident)*| $body:expr) => {{
                let $app = self.clone();
                ui.$setter(move |$($arg),*| $body);
            }};
        }
        on!(on_reset_peaks, |a| a.reset_peaks());
        on!(on_fan_mode_picked, |a, i| a.pick_fan(Fan::from_chip(i)));
        on!(on_thermal_profile_picked, |a, i| a.thermal_profile_picked(i));
        on!(on_power_picked, |a, i| a.power_picked(i));
        on!(on_kb_on_toggled, |a, on| {
            a.kb_update(|kb| kb.on = on);
            a.apply_kb(true, true);
        });
        on!(on_color_picked, |a, c| a.color_picked(from_color(c)));
        on!(on_more_colors, |a| {
            if let Some(c) = a.pick_custom_color(None) {
                a.color_picked(c);
            }
        });
        on!(on_brightness_changed, |a, v| {
            a.kb_update(|kb| kb.bright = v.clamp(0, 100) as u8);
            a.throttle(&a.kb_timer, |a| a.apply_kb(false, true));
        });
        on!(on_autostart_toggled, |a, on| {
            if settings::set_autostart(on) {
                a.clear_error();
            } else {
                a.error(
                    "Couldn't change the startup setting. Try again.",
                    "schtasks failed to change the NitroRust logon task",
                );
                a.ui.set_autostart(settings::autostart_enabled());
            }
        });
        on!(on_advanced_toggled, |a, on| {
            a.remember(&[("advanced", (on as u8).to_string())], true);
            let b = a.clone();
            Timer::single_shot(Duration::ZERO, move || b.fit_window(false));
        });
        on!(on_dismiss_error, |a| a.clear_error());
        on!(on_fixed_changed, |a, c, g| a.fixed_changed(c, g));
        on!(on_quiet_on_battery_toggled, |a, on| a.quiet_on_battery_toggled(on));
        on!(on_point_dragged, |a, i, t, p| a.point_dragged(i.max(0) as usize, t, p));
        on!(on_drag_ended, |a| {
            if std::mem::take(&mut a.st.borrow_mut().dragging) {
                a.fan_timer.stop();
                a.apply_fan(Fan::CustomCurve, true, true);
            }
        });
        on!(on_curve_text_entered, |a, t| a.curve_text_entered(&t));
        on!(on_separate_toggled, |a, on| a.separate_toggled(on));
        on!(on_curve_smooth_toggled, |a, on| a.curve_smooth_toggled(on));
        on!(on_curve_target_picked, |a, i| a.curve_target_picked(i));
        on!(on_effect_picked, |a, i| {
            a.kb_update(|kb| {
                kb.effect = i.clamp(0, proto::EFFECTS.len() as i32 - 1) as u8;
                kb.effect_mode = true;
                kb.on = true;
            });
            a.apply_kb(true, true);
        });
        on!(on_kb_speed_changed, |a, v| {
            a.kb_update(|kb| kb.speed = v.clamp(1, 9) as u8);
            if a.st.borrow().kb.effect_mode {
                a.throttle(&a.kb_timer, |a| a.apply_kb(false, true));
            }
        });
        on!(on_kb_left_toggled, |a, on| {
            a.kb_update(|kb| kb.left = on);
            if a.st.borrow().kb.effect_mode {
                a.apply_kb(true, true);
            }
        });
        on!(on_zone_picked, |a, i, c| a.zone_picked(i.max(0) as usize, from_color(c)));
        on!(on_zone_more, |a, i| {
            let i = i.clamp(0, 3) as usize;
            if let Some(c) = a.pick_custom_color(Some(i)) {
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

        let (fan, cpu, gpu, sep) =
            load_fan(&settings(&[("fan_mode", "curve"), ("cpu_curve", "50:20 80:80"), ("gpu_curve", "50:30 80:90")]));
        assert_eq!(fan, Fan::CustomCurve);
        assert!(sep);
        assert_eq!((cpu.text().as_str(), gpu.text().as_str()), ("50:20 80:80", "50:30 80:90"));

        let (_, cpu, gpu, sep) = load_fan(&settings(&[("cpu_curve", "50:20 80:80"), ("gpu_curve", "50:30 80:90"), ("gpu_separate", "0")]));
        assert!(!sep);
        assert_eq!(cpu, gpu);
    }

    #[test]
    fn chips_round_trip() {
        for i in 0..=5 {
            assert_eq!(Fan::from_chip(i).chip(), i);
        }
        assert_eq!(Fan::from_chip(5), Fan::CustomCurve);
    }

    #[test]
    fn strict_hex_parsing_and_custom_colors() {
        assert_eq!(parse_hex("#FF5A00"), Some([0xFF, 0x5A, 0x00]));
        assert_eq!(parse_hex("00a0ff"), Some([0x00, 0xA0, 0xFF]));
        assert_eq!(parse_hex("#F00"), None);
        assert_eq!(parse_hex("#FF0000FF"), None);
        assert_eq!(parse_hex("nothex"), None);

        let mut c = [0x00FF_FFFFu32; 16];
        c[0] = 0x0012_3456;
        c[15] = 0x00AB_CDEF;
        assert_eq!(parse_custom_colors(&format_custom_colors(&c)), c);
    }

    #[test]
    fn path_follows_failsafe() {
        let c = Curve::parse("55:20 65:30 75:50 85:80").unwrap();
        let p = curve_path(&c);
        assert!(p.starts_with("M 0 80 L 35.7 80"), "{p}");
        assert!(p.ends_with("L 85.7 20 L 85.7 0 L 100 0"), "{p}");

        let c = Curve::parse("55:20 85:50 99:90").unwrap();
        assert_eq!(c.percent(89), 61);
        let p = curve_path(&c);
        assert!(p.ends_with("L 84.3 39 L 85.7 39 L 85.7 0 L 100 0"), "{p}");
    }

    #[test]
    fn first_run_kb_uses_probe_instead_of_black() {
        let probe = Probe {
            kb_backlight: Ok((0, vec![3, 7, 60, 0, 2, 0x10, 0x20, 0x30])),
            ..Default::default()
        };
        let caps = Caps::from_probe("Nitro AN515-58", &probe, false);
        let kb = init_kb(&Settings::default(), &probe, Some(&caps), [[255, 0, 0]; 4]);
        assert!(kb.on && kb.effect_mode);
        assert_eq!((kb.effect, kb.speed, kb.bright, kb.left, kb.color), (3, 7, 60, true, [0x10, 0x20, 0x30]));
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
            fan_seq: 0,
            kb_seq: 0,
            profile_seq: 0,
            cpu_curve: mine.clone(),
            gpu_curve: mine.clone(),
            separate: true,
            editing_gpu: true,
            curve_smooth: true,
            fixed: (50, 50),
            dragging: false,
            kb: Kb { on: true, effect_mode: false, effect: 0, speed: 5, bright: 100, left: false, zones: [[0; 3]; 4], color: [0; 3] },
            custom_colors: [0x00FF_FFFF; 16],
            active_profile: None,
            quiet_on_battery: false,
            ac_fan_before_battery: None,
            last_ac: true,
            visible: false,
            caps: None,
            probe: Probe::default(),
            sys: SysInfo::default(),
            fans_unlocked: true,
            fan_testing: false,
            fan_test: String::new(),
            fan_test_detail: String::new(),
            last_telemetry: Telemetry::default(),
            last_tray_status: String::new(),
            last_accent: None,
            last_resume: None,
        };
        let edited = Curve::parse("60:20 88:80").unwrap();
        st.store_edit(edited.clone());
        assert_eq!(st.gpu_curve, edited);
        assert_eq!(st.cpu_curve, mine);
        st.separate = false;
        st.fan = Fan::Preset(1);
        st.store_edit(edited.clone());
        assert_eq!((st.cpu_curve.clone(), st.gpu_curve.clone()), (edited.clone(), edited));
    }
}
