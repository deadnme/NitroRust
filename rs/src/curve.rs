//! Software fan curve: temperature (°C) to fan percent, linear between points,
//! plus optional short-window spike smoothing so brief Ryzen/Intel boost spikes
//! don't rev the fans while keeping the 90 °C failsafe instantaneous.

/// At or above this temperature the fan runs at 100% whatever the curve or fixed speed says.
pub const FAILSAFE_TEMP: u32 = 90;
/// Fixed-mode failsafe releases once both sensors drop to or below this temperature.
pub const FAILSAFE_RELEASE_TEMP: u32 = 85;
/// Fans only slow down once the target is this many percent below the current speed,
/// so a temperature hovering on a boundary does not make them hunt.
pub const HYSTERESIS: u8 = 5;

/// Ready-made curves, tuned for the AN515-45: its Ryzen idles around 60-70 °C, and Auto runs
/// the fans at roughly 25-30%. Every preset still hits 100% at FAILSAFE_TEMP.
pub const PRESETS: [(&str, &str); 3] = [
    ("Quiet", "60:15 70:25 80:45 88:75"),
    ("Balanced", "55:20 65:30 75:50 85:80"),
    ("Cool", "50:30 60:45 70:65 80:90"),
];
pub const DEFAULT: &str = PRESETS[1].1;

#[derive(Clone, Debug, PartialEq)]
pub struct Curve(Vec<(u32, u8)>);

impl Curve {
    /// Parses "temp:percent" pairs separated by spaces, temperatures strictly rising.
    pub fn parse(s: &str) -> Result<Curve, String> {
        let mut points = Vec::new();
        for pair in s.split_whitespace() {
            let (t, p) = pair.split_once(':').ok_or(format!("'{pair}' is not temp:percent"))?;
            let t: u32 = t.trim().parse().map_err(|_| format!("bad temperature in '{pair}'"))?;
            let p: u8 = p.trim().trim_end_matches('%').parse().map_err(|_| format!("bad percent in '{pair}'"))?;
            if !(10..=100).contains(&p) {
                return Err(format!("'{pair}': percent must be 10-100"));
            }
            if t > 110 {
                return Err(format!("'{pair}': temperature must be 0-110"));
            }
            if points.last().is_some_and(|&(lt, _)| t <= lt) {
                return Err("temperatures must rise from left to right".into());
            }
            points.push((t, p));
        }
        if points.len() < 2 {
            return Err("need at least two points".into());
        }
        Ok(Curve(points))
    }

    pub fn points(&self) -> &[(u32, u8)] {
        &self.0
    }

    pub fn text(&self) -> String {
        self.0.iter().map(|(t, p)| format!("{t}:{p}")).collect::<Vec<_>>().join(" ")
    }

    pub fn percent(&self, temp: u32) -> u8 {
        if temp >= FAILSAFE_TEMP {
            return 100;
        }
        let pts = &self.0;
        if temp <= pts[0].0 {
            return pts[0].1;
        }
        for w in pts.windows(2) {
            let ((t0, p0), (t1, p1)) = (w[0], w[1]);
            if temp <= t1 {
                let (p0, p1) = (p0 as i32, p1 as i32);
                return (p0 + (p1 - p0) * (temp - t0) as i32 / (t1 - t0) as i32) as u8;
            }
        }
        pts[pts.len() - 1].1
    }
}

/// The speed to send, or None to leave the fan alone. Speeds rise at once and fall with hysteresis.
pub fn next_speed(current: u8, target: u8) -> Option<u8> {
    if target > current || current >= target + HYSTERESIS {
        Some(target)
    } else {
        None
    }
}

/// 3-sample (~4-6 s) rolling mean to damp brief CPU boost spikes without delaying the 90 °C failsafe.
#[derive(Clone, Debug, Default)]
pub struct TempSmoother {
    samples: [u32; 3],
    len: usize,
    idx: usize,
}

impl TempSmoother {
    pub fn reset(&mut self) {
        self.len = 0;
        self.idx = 0;
    }

    pub fn push(&mut self, raw: u32, enabled: bool) -> u32 {
        if raw == 0 {
            self.reset();
            return 0;
        }
        self.samples[self.idx] = raw;
        self.idx = (self.idx + 1) % self.samples.len();
        if self.len < self.samples.len() {
            self.len += 1;
        }
        if !enabled || raw >= FAILSAFE_TEMP {
            return raw;
        }
        let sum: u32 = self.samples[..self.len].iter().sum();
        (sum + self.len as u32 / 2) / self.len as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve() {
        for (_, text) in PRESETS {
            assert!(Curve::parse(text).is_ok(), "{text}");
        }
        let c = Curve::parse("50:20 60:35 70:55 80:80 90:100").unwrap();
        assert_eq!(c.percent(30), 20);
        assert_eq!(c.percent(55), 27);
        assert_eq!(c.percent(80), 80);
        assert_eq!(c.percent(89), 98);
        assert_eq!(c.percent(95), 100);
        let bal = Curve::parse(DEFAULT).unwrap();
        assert_eq!(bal.percent(65), 30); // typical idle: close to Auto
        assert_eq!(bal.percent(89), 80); // flat after the last point...
        assert_eq!(bal.percent(90), 100); // ...until the failsafe
        assert_eq!(Curve::parse(&c.text()).unwrap(), c);
        let low = Curve::parse("40:10 70:30").unwrap();
        assert_eq!(low.percent(85), 30); // flat after the last point
        assert_eq!(low.percent(90), 100); // failsafe
        assert!(Curve::parse("60:20 50:30").is_err());
        assert!(Curve::parse("60:5 70:30").is_err());
        assert!(Curve::parse("60:20").is_err());
        assert!(Curve::parse("60-20 70:30").is_err());
        assert_eq!(next_speed(0, 20), Some(20));
        assert_eq!(next_speed(50, 47), None);
        assert_eq!(next_speed(50, 45), Some(45));
        assert_eq!(next_speed(50, 50), None);
    }

    #[test]
    fn smoother_damps_spikes_but_never_delays_failsafe() {
        let mut s = TempSmoother::default();
        assert_eq!(s.push(60, true), 60);
        assert_eq!(s.push(60, true), 60);
        // Single 18 C spike is damped to 66 C instead of jumping straight to 78 C.
        assert_eq!(s.push(78, true), 66);
        // Back to idle: settles cleanly once the 3-sample window flushes.
        assert_eq!(s.push(60, true), 66);
        assert_eq!(s.push(60, true), 66);
        assert_eq!(s.push(60, true), 60);
        // Failsafe bypasses smoothing immediately.
        assert_eq!(s.push(91, true), 91);
        // Disabled smoothing passes raw through.
        assert_eq!(s.push(77, false), 77);
        // Sleeping/failed sensor resets window.
        assert_eq!(s.push(0, true), 0);
        assert_eq!(s.push(55, true), 55);
    }
}
