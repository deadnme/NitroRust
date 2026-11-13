//! What this laptop supports, worked out at startup from read-only firmware answers.
//! Decoding follows the Linux acer-wmi driver (GetGamingSysInfo 0x0000 sensor mask,
//! GetGamingFanBehavior status) so it carries over to other Nitro and Predator models.
//! Anything that can't be decoded counts as "not supported".

/// Models someone has tested NitroRust on. Others run in safe mode until reported.
pub const VERIFIED: &[&str] = &["Nitro AN515-45"];

/// Raw answers from the read-only probe. None = the call failed.
#[derive(Clone, Debug, Default)]
pub struct Probe {
    /// GetGamingSysInfo(0x0000): status in bits 0-7, supported-sensor mask in bits 24-39.
    pub sys_info: Option<u64>,
    /// GetGamingFanBehavior(CPU fan): status in bits 0-7.
    pub fan_behavior: Option<u64>,
    /// GetGamingKBBacklight: return code and the 16-byte state.
    pub kb_backlight: Option<(u8, Vec<u8>)>,
    /// GetGamingRgbKb(zone 1): status in bits 0-7.
    pub rgb_zone: Option<u64>,
    /// GetGamingMiscSetting(0x0A): firmware power profiles (logged only).
    pub misc_profiles: Option<u64>,
}

/// Sensor IDs from GetGamingSysInfo; bit (id - 1) of the mask.
const SENSOR_CPU_TEMP: u32 = 1;
const SENSOR_CPU_FAN: u32 = 2;
const SENSOR_GPU_FAN: u32 = 6;
const SENSOR_GPU_TEMP: u32 = 10;

#[derive(Clone, Debug, PartialEq)]
pub struct Caps {
    pub model: String,
    pub verified: bool,
    pub cpu_temp: bool,
    pub gpu_temp: bool,
    pub cpu_rpm: bool,
    pub gpu_rpm: bool,
    /// Fans the firmware will be told about (SetGamingFanBehavior bits / SetGamingFanSpeed IDs).
    pub cpu_fan: bool,
    pub gpu_fan: bool,
    /// The firmware answers fan-mode requests (Auto/Max always; the rest needs `fans_unlocked`).
    pub fan_control: bool,
    /// Keyboard backlight (on/off, colour, brightness, effects).
    pub kb: bool,
    /// Per-zone colours. Only on verified models: the byte layout differs between models.
    pub zones: bool,
}

fn status_ok(raw: Option<u64>) -> Option<u64> {
    raw.filter(|v| v & 0xFF == 0)
}

/// Supported-sensor mask, or None when the firmware doesn't report one.
pub fn sensor_mask(sys_info: Option<u64>) -> Option<u16> {
    let mask = ((status_ok(sys_info)? >> 24) & 0xFFFF) as u16;
    (mask != 0).then_some(mask)
}

fn has(mask: Option<u16>, id: u32) -> bool {
    // No mask: older firmware; assume the standard Nitro sensors, as before.
    mask.map_or(true, |m| m & (1 << (id - 1)) != 0)
}

impl Caps {
    pub fn from_probe(model: &str, p: &Probe, pretend_unverified: bool) -> Caps {
        let mask = sensor_mask(p.sys_info);
        let fan_control = status_ok(p.fan_behavior).is_some();
        let kb = matches!(&p.kb_backlight, Some((0, bytes)) if bytes.len() >= 4);
        let verified = !pretend_unverified && VERIFIED.iter().any(|m| m.eq_ignore_ascii_case(model.trim()));
        let cpu_rpm = has(mask, SENSOR_CPU_FAN);
        let gpu_rpm = has(mask, SENSOR_GPU_FAN);
        Caps {
            model: model.trim().to_string(),
            verified,
            cpu_temp: has(mask, SENSOR_CPU_TEMP),
            gpu_temp: has(mask, SENSOR_GPU_TEMP),
            cpu_rpm,
            gpu_rpm,
            // A fan with an RPM sensor exists. With neither reported, keep the usual pair.
            cpu_fan: cpu_rpm || !gpu_rpm,
            gpu_fan: gpu_rpm || !cpu_rpm,
            fan_control,
            kb,
            zones: kb && verified && status_ok(p.rgb_zone).is_some(),
        }
    }

    /// Key for the saved fan-test pass: a new model or BIOS needs a new test.
    pub fn test_key(&self, bios: &str) -> String {
        format!("pass:{}:{}", self.model, bios.trim())
    }
}

fn hex(v: Option<u64>) -> String {
    v.map_or("call failed".into(), |v| format!("0x{v:016X}"))
}

fn sensor_names(mask: Option<u16>) -> String {
    let Some(m) = mask else { return "none reported".into() };
    (1..=16u32)
        .filter(|id| m & (1 << (id - 1)) != 0)
        .map(|id| match id {
            SENSOR_CPU_TEMP => "CPU temp".to_string(),
            SENSOR_CPU_FAN => "CPU fan".to_string(),
            SENSOR_GPU_FAN => "GPU fan".to_string(),
            SENSOR_GPU_TEMP => "GPU temp".to_string(),
            other => format!("sensor {other}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn yes(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

/// Plain-text report for a GitHub issue. No user name, serial number or paths.
pub struct Report<'a> {
    pub version: &'a str,
    pub windows: &'a str,
    pub maker: &'a str,
    pub bios: &'a str,
    pub caps: &'a Caps,
    pub probe: &'a Probe,
    pub fan_test: &'a str,
    pub now: &'a str,
}

impl Report<'_> {
    pub fn text(&self) -> String {
        let c = self.caps;
        let p = self.probe;
        let kb = match &p.kb_backlight {
            Some((r, b)) => format!("return {r}, bytes {}", b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ")),
            None => "call failed".into(),
        };
        format!(
            "NitroRust {}\n\
             Windows: {}\n\
             Maker: {}\n\
             Model: {}\n\
             BIOS: {}\n\
             Verified model: {}\n\
             \n\
             Sensors (GetGamingSysInfo 0x0000): {} -> {}\n\
             Fan behaviour (GetGamingFanBehavior 0x1): {}\n\
             Keyboard backlight (GetGamingKBBacklight): {}\n\
             Keyboard zone 1 (GetGamingRgbKb 0x1): {}\n\
             Power profiles (GetGamingMiscSetting 0x0A): {}\n\
             \n\
             Detected: fan control {}, CPU fan {}, GPU fan {}, keyboard {}, zone colours {}\n\
             Fan test: {}\n\
             Now: {}\n",
            self.version,
            self.windows,
            self.maker,
            c.model,
            self.bios,
            yes(c.verified),
            hex(p.sys_info),
            sensor_names(sensor_mask(p.sys_info)),
            hex(p.fan_behavior),
            kb,
            hex(p.rgb_zone),
            hex(p.misc_profiles),
            yes(c.fan_control),
            yes(c.cpu_fan),
            yes(c.gpu_fan),
            yes(c.kb),
            yes(c.zones),
            self.fan_test,
            self.now,
        )
    }
}

/// Fan-test verdict from RPM at a low and a high speed (per fan; None = no RPM sensor).
/// Passes when every fan with a sensor clearly speeds up. A fan that ignores the command stays
/// within reading noise (about 100 rpm); on the AN515-45 the smallest real rise seen was 767 rpm.
pub fn fan_test_passed(low: (Option<u32>, Option<u32>), high: (Option<u32>, Option<u32>)) -> bool {
    const MIN_RISE: u32 = 300;
    let fans = [(low.0, high.0), (low.1, high.1)];
    let measured: Vec<_> = fans.iter().filter_map(|&(l, h)| Some((l?, h?))).collect();
    !measured.is_empty() && measured.iter().all(|&(l, h)| h >= l + MIN_RISE)
}

/// Percent-encodes a string for a URL query value.
pub fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Answers read from the AN515-45 (BIOS V1.14) on 2026-11-13.
    fn an515_45() -> Probe {
        Probe {
            sys_info: Some(0x2_2700_0000),
            fan_behavior: Some(0x100),
            kb_backlight: Some((0, vec![0, 0, 0x64, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
            rgb_zone: Some(0xFF00),
            misc_profiles: Some(0x1),
        }
    }

    #[test]
    fn an515_45_is_fully_supported() {
        let c = Caps::from_probe("Nitro AN515-45", &an515_45(), false);
        assert!(c.verified && c.fan_control && c.kb && c.zones);
        assert!(c.cpu_temp && c.gpu_temp && c.cpu_rpm && c.gpu_rpm && c.cpu_fan && c.gpu_fan);
        assert_eq!(sensor_names(sensor_mask(an515_45().sys_info)), "CPU temp, CPU fan, sensor 3, GPU fan, GPU temp");
    }

    #[test]
    fn unverified_model_hides_zones_only() {
        let c = Caps::from_probe("Nitro AN515-58", &an515_45(), false);
        assert!(!c.verified && !c.zones);
        assert!(c.fan_control && c.kb);
        let pretend = Caps::from_probe("Nitro AN515-45", &an515_45(), true);
        assert!(!pretend.verified && !pretend.zones);
    }

    #[test]
    fn refusals_and_failures_turn_features_off() {
        let p = Probe { sys_info: Some(0x1), fan_behavior: Some(0x3), kb_backlight: Some((1, vec![0; 16])), rgb_zone: None, misc_profiles: None };
        let c = Caps::from_probe("Aspire 7", &p, false);
        assert!(!c.fan_control && !c.kb && !c.zones);
        // No sensor mask: fall back to the standard sensors and both fans.
        assert!(c.cpu_temp && c.gpu_temp && c.cpu_fan && c.gpu_fan);
        assert!(!Caps::from_probe("x", &Probe::default(), false).fan_control);
    }

    #[test]
    fn single_fan_model() {
        // CPU temp + CPU fan only.
        let p = Probe { sys_info: Some(0x3_000000), ..an515_45() };
        let c = Caps::from_probe("Nitro V", &p, false);
        assert!(c.cpu_fan && !c.gpu_fan && !c.gpu_temp && !c.gpu_rpm);
    }

    #[test]
    fn fan_test_verdicts() {
        assert!(fan_test_passed((Some(2000), Some(2100)), (Some(5000), Some(5200))));
        assert!(!fan_test_passed((Some(2000), Some(2100)), (Some(5000), Some(2300))));
        // Measured on the AN515-45: the GPU fan had not slowed much at 30% after 5 s.
        assert!(fan_test_passed((Some(3947), Some(4687)), (Some(5769), Some(5454))));
        assert!(fan_test_passed((Some(1500), None), (Some(4800), None)));
        assert!(!fan_test_passed((None, None), (None, None)));
    }

    #[test]
    fn report_and_url() {
        let c = Caps::from_probe("Nitro AN515-45", &an515_45(), false);
        let r = Report { version: "1.1.0", windows: "10.0.26300", maker: "Acer", bios: "V1.14", caps: &c, probe: &an515_45(), fan_test: "not needed", now: "CPU 54 °C" };
        let t = r.text();
        assert!(t.contains("Model: Nitro AN515-45") && t.contains("0x0000000227000000") && t.contains("zone colours yes"));
        assert_eq!(url_encode("Nitro AN515-58 (V1.0)"), "Nitro%20AN515-58%20%28V1.0%29");
    }
}
