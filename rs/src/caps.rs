//! What this laptop supports, worked out at startup from read-only firmware answers.
//! Decoding follows the Linux acer-wmi driver (GetGamingSysInfo 0x0000 sensor mask,
//! GetGamingFanBehavior status, GetGamingMiscSetting 0x0A thermal profiles) so it carries
//! over to other Nitro and Predator models. Anything that can't be decoded counts as
//! "not supported".

use crate::proto;

/// Models someone has tested NitroRust on. Others run in safe mode until reported.
pub const VERIFIED: &[&str] = &["Nitro AN515-45"];

/// Raw answers from the read-only probe. Err holds the exact WMI error for diagnostics.
#[derive(Clone, Debug)]
pub struct Probe {
    /// GetGamingSysInfo(0x0000): status in bits 0-7, supported-sensor mask in bits 24-39.
    pub sys_info: Result<u64, String>,
    /// GetGamingFanBehavior(CPU fan): status in bits 0-7.
    pub fan_behavior: Result<u64, String>,
    /// GetGamingKBBacklight: return code and the 16-byte state.
    pub kb_backlight: Result<(u8, Vec<u8>), String>,
    /// GetGamingRgbKb(zone 1): status in bits 0-7.
    pub rgb_zone: Result<u64, String>,
    /// GetGamingMiscSetting(0x0A): firmware thermal profiles bitmask.
    pub misc_profiles: Result<u64, String>,
    /// GetGamingMiscSetting(0x0B): currently active firmware thermal profile.
    pub active_profile: Result<u64, String>,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            sys_info: Err("not probed".into()),
            fan_behavior: Err("not probed".into()),
            kb_backlight: Err("not probed".into()),
            rgb_zone: Err("not probed".into()),
            misc_profiles: Err("not probed".into()),
            active_profile: Err("not probed".into()),
        }
    }
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
    /// Firmware thermal/power profiles reported by GetGamingMiscSetting(0x0A).
    pub profiles: Vec<(u8, &'static str)>,
}

fn status_ok(raw: &Result<u64, String>) -> Option<u64> {
    raw.as_ref().ok().copied().filter(|v| v & 0xFF == 0)
}

/// Supported-sensor mask, or None when the firmware doesn't report one.
pub fn sensor_mask(sys_info: &Result<u64, String>) -> Option<u16> {
    let mask = ((status_ok(sys_info)? >> 24) & 0xFFFF) as u16;
    (mask != 0).then_some(mask)
}

fn has(mask: Option<u16>, id: u32) -> bool {
    // No mask: older firmware; assume the standard Nitro sensors, as before.
    mask.is_none_or(|m| m & (1 << (id - 1)) != 0)
}

impl Caps {
    pub fn from_probe(model: &str, p: &Probe, pretend_unverified: bool) -> Caps {
        let mask = sensor_mask(&p.sys_info);
        let fan_control = status_ok(&p.fan_behavior).is_some();
        let kb = matches!(&p.kb_backlight, Ok((0, bytes)) if bytes.len() >= 4);
        let verified = !pretend_unverified && VERIFIED.iter().any(|m| m.eq_ignore_ascii_case(model.trim()));
        let cpu_rpm = has(mask, SENSOR_CPU_FAN);
        let gpu_rpm = has(mask, SENSOR_GPU_FAN);
        let profiles = p.misc_profiles.as_ref().ok().map_or_else(Vec::new, |&raw| proto::supported_profiles(raw));
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
            zones: kb && verified && status_ok(&p.rgb_zone).is_some(),
            profiles,
        }
    }

    /// Key for the saved fan-test pass: a new model or BIOS needs a new test.
    pub fn test_key(&self, bios: &str) -> String {
        format!("pass:{}:{}", self.model, bios.trim())
    }
}

fn hex(v: &Result<u64, String>) -> String {
    match v {
        Ok(v) => format!("0x{v:016X}"),
        Err(e) => format!("call failed ({e})"),
    }
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
            Ok((r, b)) => format!(
                "return {r}, bytes {}",
                b.iter().map(|x| format!("{x:02X}")).collect::<Vec<_>>().join(" ")
            ),
            Err(e) => format!("call failed ({e})"),
        };
        let profiles_desc = if c.profiles.is_empty() {
            "none".to_string()
        } else {
            c.profiles.iter().map(|(_, n)| *n).collect::<Vec<_>>().join(", ")
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
             Power profiles (GetGamingMiscSetting 0x0A): {} -> {}\n\
             Active profile (GetGamingMiscSetting 0x0B): {}\n\
             \n\
             Detected: fan control {}, CPU fan {}, GPU fan {}, keyboard {}, zone colours {}, thermal profiles {}\n\
             Fan test: {}\n\
             Now: {}\n",
            self.version,
            self.windows,
            self.maker,
            c.model,
            self.bios,
            yes(c.verified),
            hex(&p.sys_info),
            sensor_names(sensor_mask(&p.sys_info)),
            hex(&p.fan_behavior),
            kb,
            hex(&p.rgb_zone),
            hex(&p.misc_profiles),
            profiles_desc,
            hex(&p.active_profile),
            yes(c.fan_control),
            yes(c.cpu_fan),
            yes(c.gpu_fan),
            yes(c.kb),
            yes(c.zones),
            yes(!c.profiles.is_empty()),
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
            sys_info: Ok(0x2_2700_0000),
            fan_behavior: Ok(0x100),
            kb_backlight: Ok((0, vec![0, 0, 0x64, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])),
            rgb_zone: Ok(0xFF00),
            misc_profiles: Ok(0x1),
            active_profile: Err("not supported".into()),
        }
    }

    #[test]
    fn an515_45_is_fully_supported() {
        let c = Caps::from_probe("Nitro AN515-45", &an515_45(), false);
        assert!(c.verified && c.fan_control && c.kb && c.zones && c.profiles.is_empty());
        assert!(c.cpu_temp && c.gpu_temp && c.cpu_rpm && c.gpu_rpm && c.cpu_fan && c.gpu_fan);
        assert_eq!(sensor_names(sensor_mask(&an515_45().sys_info)), "CPU temp, CPU fan, sensor 3, GPU fan, GPU temp");
    }

    #[test]
    fn anv16_71_detects_thermal_profiles_and_error_reason() {
        let p = Probe {
            sys_info: Ok(0x2_2700_0000),
            fan_behavior: Ok(0x100),
            kb_backlight: Err("WBEM_E_NOT_FOUND".into()),
            rgb_zone: Ok(0),
            misc_profiles: Ok(0x7300),
            active_profile: Ok(0x0100),
        };
        let c = Caps::from_probe("Nitro ANV16-71", &p, false);
        assert!(!c.verified && c.fan_control && !c.kb && !c.zones);
        assert_eq!(c.profiles.len(), 5);
        let r = Report {
            version: "1.2.0",
            windows: "build 26100",
            maker: "Acer",
            bios: "V1.09",
            caps: &c,
            probe: &p,
            fan_test: "passed",
            now: "CPU 65 °C",
        }
        .text();
        assert!(r.contains("WBEM_E_NOT_FOUND"));
        assert!(r.contains("Quiet, Balanced, Performance, Turbo, Eco"));
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
        let p = Probe {
            sys_info: Ok(0x1),
            fan_behavior: Ok(0x3),
            kb_backlight: Ok((1, vec![0; 16])),
            rgb_zone: Err("unsupported".into()),
            misc_profiles: Err("unsupported".into()),
            active_profile: Err("unsupported".into()),
        };
        let c = Caps::from_probe("Aspire 7", &p, false);
        assert!(!c.fan_control && !c.kb && !c.zones && c.profiles.is_empty());
        // No sensor mask: fall back to the standard sensors and both fans.
        assert!(c.cpu_temp && c.gpu_temp && c.cpu_fan && c.gpu_fan);
        assert!(!Caps::from_probe("x", &Probe::default(), false).fan_control);
    }

    #[test]
    fn single_fan_model() {
        // CPU temp + CPU fan only.
        let p = Probe { sys_info: Ok(0x3_000000), ..an515_45() };
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
