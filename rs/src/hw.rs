//! Dedicated Acer WMI hardware thread: telemetry polling, software fan curves (with
//! optional spike smoothing), 90 °C safety override for Fixed mode, cancellable safe
//! fan test, and non-blocking async WMI writes so the UI thread never stalls.

use crate::acer::{Acer, Telemetry};
use crate::caps::{self, Caps, Probe};
use crate::curve::{self, Curve, TempSmoother};
use crate::AppWindow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often the hardware thread reads sensors and runs the curve/failsafe loop.
pub const POLL_PERIOD: Duration = Duration::from_secs(2);

/// Refused curve writes in a row before the curve is stopped and fans handed back to Auto.
const CURVE_REFUSALS: u32 = 2;

/// Refuse to start the 30% low-speed fan test if either sensor is already at or above this temp.
const FAN_TEST_MAX_START_TEMP: u32 = 80;

type Job = Box<dyn FnOnce(&Acer) + Send>;

#[derive(Clone, Debug)]
pub enum HwFanMode {
    Off,
    Curve { cpu: Curve, gpu: Curve, smooth: bool },
    Fixed { cpu: u8, gpu: u8 },
}

enum Msg {
    Job(Job),
    FanMode(HwFanMode),
    Resume,
}

/// Startup snapshot returned once the WMI connection and probe finish.
pub struct HwInit {
    pub hw: Hw,
    pub zones: [[u8; 3]; 4],
    pub caps: Caps,
    pub probe: Probe,
    pub active_profile: Option<u8>,
}

/// Handle to the hardware worker thread.
pub struct Hw {
    tx: std::sync::mpsc::Sender<Msg>,
    cancel_test: Arc<AtomicBool>,
}

impl Hw {
    /// Opens WMI on a dedicated worker thread, runs the read-only probe, and starts polling.
    pub fn open(model: String, pretend_unverified: bool, ui: slint::Weak<AppWindow>) -> Result<HwInit, String> {
        let (tx, rx) = std::sync::mpsc::channel::<Msg>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let cancel_test = Arc::new(AtomicBool::new(false));

        std::thread::spawn(move || {
            let mut a = match Acer::open() {
                Ok(a) => a,
                Err(e) => return drop(ready_tx.send(Err(e))),
            };
            let probe = a.probe();
            let caps = Caps::from_probe(&model, &probe, pretend_unverified);
            let active_profile = probe
                .active_profile
                .as_ref()
                .ok()
                .and_then(|&raw| crate::proto::active_profile(raw));
            a.caps = Some(caps.clone());
            // Only read per-zone RGB colours when the laptop actually supports them.
            let zones = if caps.zones { a.zone_colors() } else { [[255, 0, 0]; 4] };
            let _ = ready_tx.send(Ok((zones, caps, probe, active_profile)));

            let ui_t = ui.clone();
            let on_telemetry = move |t: Telemetry| {
                let _ = ui_t.upgrade_in_event_loop(move |ui| {
                    ui.set_cpu_temp(t.cpu_temp as i32);
                    ui.set_gpu_temp(t.gpu_temp as i32);
                    ui.set_cpu_peak(ui.get_cpu_peak().max(t.cpu_temp as i32));
                    ui.set_gpu_peak(ui.get_gpu_peak().max(t.gpu_temp as i32));
                    ui.set_cpu_rpm(t.cpu_rpm as i32);
                    ui.set_gpu_rpm(t.gpu_rpm as i32);
                    ui.invoke_tray_action("telemetry".into());
                });
            };
            let ui_r = ui.clone();
            let on_refused = move || {
                let _ = ui_r.upgrade_in_event_loop(|ui| ui.invoke_tray_action("curve-refused".into()));
            };
            let ui_f = ui;
            let on_fixed_failsafe = move |active: bool| {
                let _ = ui_f.upgrade_in_event_loop(move |ui| {
                    ui.set_fixed_failsafe_active(active);
                });
            };
            hw_loop(&a, rx, on_telemetry, on_refused, on_fixed_failsafe);
        });

        let (zones, caps, probe, active_profile) = ready_rx.recv().map_err(|e| e.to_string())??;
        Ok(HwInit {
            hw: Hw { tx, cancel_test },
            zones,
            caps,
            probe,
            active_profile,
        })
    }

    /// Queues a job without blocking the UI thread.
    pub fn spawn(&self, f: impl FnOnce(&Acer) + Send + 'static) {
        let _ = self.tx.send(Msg::Job(Box::new(f)));
    }

    /// Runs a synchronous job on the hardware thread (used only during clean shutdown).
    pub fn run_sync<T: Send + 'static>(&self, f: impl FnOnce(&Acer) -> T + Send + 'static) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.tx.send(Msg::Job(Box::new(move |a| drop(tx.send(f(a)))))).ok()?;
        rx.recv().ok()
    }

    pub fn set_fan_mode(&self, mode: HwFanMode) {
        let _ = self.tx.send(Msg::FanMode(mode));
    }

    /// Notifies the hardware loop that Windows resumed from sleep/hibernate.
    pub fn notify_resume(&self) {
        let _ = self.tx.send(Msg::Resume);
    }

    pub fn cancel_token(&self) -> Arc<AtomicBool> {
        self.cancel_test.clone()
    }

    pub fn cancel_fan_test(&self) {
        self.cancel_test.store(true, Ordering::Relaxed);
    }

    pub fn reset_fan_test_cancel(&self) {
        self.cancel_test.store(false, Ordering::Relaxed);
    }
}

/// Target speeds (CPU, GPU) for these readings. The GPU reads 0 °C when the dGPU sleeps, when
/// a read fails, or on a model with no GPU sensor: its fan then follows the CPU temperature,
/// so the 90 °C failsafe still applies (shared heat pipes) instead of the curve's lowest point.
pub fn curve_targets(t: Telemetry, cc: &Curve, gc: &Curve) -> (u8, u8) {
    let gpu_temp = if t.gpu_temp > 0 { t.gpu_temp } else { t.cpu_temp };
    (cc.percent(t.cpu_temp), gc.percent(gpu_temp))
}

/// Fixed-mode safety transition: enters failsafe at >= 90 °C, releases once cool (<= 85 °C).
/// Never releases an active failsafe on a failed CPU sensor read (`cpu_temp == 0`).
pub fn fixed_failsafe_next(currently_active: bool, has_cpu_temp: bool, t: Telemetry) -> bool {
    let hot = t.cpu_temp >= curve::FAILSAFE_TEMP || t.gpu_temp >= curve::FAILSAFE_TEMP;
    if hot {
        return true;
    }
    if currently_active {
        // A sleeping dGPU legitimately reads 0 °C; a running CPU reading 0 °C is a failed read.
        let cool_cpu = if has_cpu_temp {
            t.cpu_temp > 0 && t.cpu_temp <= curve::FAILSAFE_RELEASE_TEMP
        } else {
            true
        };
        let cool_gpu = t.gpu_temp == 0 || t.gpu_temp <= curve::FAILSAFE_RELEASE_TEMP;
        return !(cool_cpu && cool_gpu);
    }
    false
}

fn run_curve(
    a: &Acer,
    t: Telemetry,
    cc: &Curve,
    gc: &Curve,
    (cur_cpu, cur_gpu): (u8, u8),
) -> Option<(u8, u8)> {
    if t.cpu_temp == 0 {
        // The CPU sensor read failed: hand control back rather than guess.
        let _ = a.fans_auto();
        return Some((0, 0));
    }
    let (cpu_target, gpu_target) = curve_targets(t, cc, gc);
    let cpu = curve::next_speed(cur_cpu, cpu_target);
    let gpu = curve::next_speed(cur_gpu, gpu_target);
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

fn hw_loop(
    a: &Acer,
    rx: std::sync::mpsc::Receiver<Msg>,
    on_telemetry: impl Fn(Telemetry),
    on_refused: impl Fn(),
    on_fixed_failsafe: impl Fn(bool),
) {
    use std::sync::mpsc::RecvTimeoutError;

    let mut mode = HwFanMode::Off;
    let mut sent = (0u8, 0u8);
    let mut cpu_smooth = TempSmoother::default();
    let mut gpu_smooth = TempSmoother::default();
    let mut fixed_failsafe = false;
    let mut next = Instant::now();
    let mut refusals = 0;

    loop {
        match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
            Ok(Msg::Job(job)) => job(a),
            Ok(Msg::FanMode(m)) => {
                if fixed_failsafe && !matches!(m, HwFanMode::Fixed { .. }) {
                    fixed_failsafe = false;
                    on_fixed_failsafe(false);
                }
                mode = m;
                sent = (0, 0);
                refusals = 0;
                cpu_smooth.reset();
                gpu_smooth.reset();
                next = Instant::now(); // apply immediately
            }
            Ok(Msg::Resume) => {
                // EC resets after sleep/hibernate: force re-send on the next poll tick.
                sent = (0, 0);
                refusals = 0;
                cpu_smooth.reset();
                gpu_smooth.reset();
                if fixed_failsafe {
                    fixed_failsafe = false;
                    on_fixed_failsafe(false);
                }
                next = Instant::now();
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

        match &mode {
            HwFanMode::Off => {}
            HwFanMode::Curve { cpu: cc, gpu: gc, smooth } => {
                let effective = Telemetry {
                    cpu_temp: cpu_smooth.push(t.cpu_temp, *smooth),
                    gpu_temp: gpu_smooth.push(t.gpu_temp, *smooth),
                    ..t
                };
                match run_curve(a, effective, cc, gc, sent) {
                    Some(s) => {
                        sent = s;
                        refusals = 0;
                    }
                    None => {
                        sent = (0, 0);
                        refusals += 1;
                        if refusals >= CURVE_REFUSALS {
                            mode = HwFanMode::Off;
                            let _ = a.fans_auto();
                            on_refused();
                        }
                    }
                }
            }
            HwFanMode::Fixed { cpu, gpu } => {
                let has_cpu_temp = a.caps.as_ref().is_none_or(|c| c.cpu_temp);
                let should_override = fixed_failsafe_next(fixed_failsafe, has_cpu_temp, t);
                if should_override && !fixed_failsafe && matches!(a.fans_custom(100, 100), Ok(true)) {
                    fixed_failsafe = true;
                    on_fixed_failsafe(true);
                } else if !should_override && fixed_failsafe && matches!(a.fans_custom(*cpu, *gpu), Ok(true)) {
                    fixed_failsafe = false;
                    on_fixed_failsafe(false);
                }
            }
        }
    }
}

/// Safe, cancellable fan test (hardware thread).
/// - Refuses to start if either sensor is >= 80 °C.
/// - Polls in 250 ms steps so exit cancels immediately and any >= 90 °C spike aborts to Auto.
pub fn fan_test(a: &Acer, caps: &Caps, cancel: &AtomicBool) -> (bool, String) {
    let initial = a.telemetry();
    if initial.cpu_temp >= FAN_TEST_MAX_START_TEMP || initial.gpu_temp >= FAN_TEST_MAX_START_TEMP {
        return (
            false,
            format!(
                "laptop is too warm to run the 30% test (CPU {} °C, GPU {} °C); let it cool below {} °C first",
                initial.cpu_temp, initial.gpu_temp, FAN_TEST_MAX_START_TEMP
            ),
        );
    }

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
                let deadline = Instant::now() + Duration::from_secs(6);
                while Instant::now() < deadline {
                    if cancel.load(Ordering::Relaxed) {
                        return Err("cancelled".into());
                    }
                    let t = a.telemetry();
                    if t.cpu_temp >= curve::FAILSAFE_TEMP || t.gpu_temp >= curve::FAILSAFE_TEMP {
                        return Err(format!(
                            "aborted for safety (CPU {} °C, GPU {} °C)",
                            t.cpu_temp, t.gpu_temp
                        ));
                    }
                    let rem = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(rem.min(Duration::from_millis(250)));
                }
                Ok(read())
            }
            Ok(false) => Err("the laptop refused the speed change".into()),
            Err(e) => Err(e),
        }
    };

    let result = hold(30).and_then(|low| Ok((low, hold(100)?)));
    let _ = a.fans_auto();
    match result {
        Ok((low, _)) if low.0.is_none() && low.1.is_none() => (false, "no fan speed sensors to check".into()),
        Ok((low, high)) => (
            caps::fan_test_passed(low, high),
            format!("30%: {}; 100%: {}", show(low), show(high)),
        ),
        Err(e) => (false, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_fan_keeps_failsafe_without_gpu_reading() {
        let c = Curve::parse("50:20 60:35 70:55 80:80 90:100").unwrap();
        let t = |cpu, gpu| Telemetry { cpu_temp: cpu, gpu_temp: gpu, ..Default::default() };
        assert_eq!(curve_targets(t(92, 0), &c, &c), (100, 100));
        assert_eq!(curve_targets(t(92, 50), &c, &c), (100, 20));
        assert_eq!(curve_targets(t(50, 95), &c, &c), (20, 100));
    }

    #[test]
    fn fixed_failsafe_triggers_at_90_and_releases_at_85() {
        let t = |cpu, gpu| Telemetry { cpu_temp: cpu, gpu_temp: gpu, ..Default::default() };
        assert!(!fixed_failsafe_next(false, true, t(89, 89)));
        assert!(fixed_failsafe_next(false, true, t(90, 60)));
        assert!(fixed_failsafe_next(false, true, t(60, 91)));
        // Stays active between 86 and 89 C (hysteresis).
        assert!(fixed_failsafe_next(true, true, t(88, 60)));
        assert!(fixed_failsafe_next(true, true, t(70, 86)));
        // A failed CPU sensor read (0 C) must NOT release an active failsafe.
        assert!(fixed_failsafe_next(true, true, t(0, 65)));
        // Releases once both sensors <= 85 C (or GPU sleeping at 0).
        assert!(!fixed_failsafe_next(true, true, t(85, 0)));
        assert!(!fixed_failsafe_next(true, true, t(85, 85)));
        // On a model with no CPU temp sensor, GPU cooling releases failsafe.
        assert!(!fixed_failsafe_next(true, false, t(0, 80)));
    }
}
