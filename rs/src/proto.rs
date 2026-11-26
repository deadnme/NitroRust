//! Acer AcerGamingFunction (root\WMI) payloads. Verified on the Nitro AN515-45; the fan,
//! sensor and thermal profile encodings match the Linux acer-wmi driver across Nitro/Predator
//! models. Values come from Acer's own software as captured by the Linux driver (acer-wmi /
//! Linuwu-Sense) and verified by read-only probes on real hardware.

/// SetGamingFanBehavior modes.
pub const MODE_AUTO: u64 = 1;
pub const MODE_MAX: u64 = 2;
pub const MODE_CUSTOM: u64 = 3;

/// SetGamingFanBehavior payload: fan bits (CPU = bit 0, GPU = bit 3) and the mode for each fan
/// (CPU in bits 16-17, GPU in bits 22-23).
pub fn fan_behavior(cpu: bool, gpu: bool, mode: u64) -> u64 {
    let mut v = 0;
    if cpu {
        v |= 1 | mode << 16;
    }
    if gpu {
        v |= 1 << 3 | mode << 22;
    }
    v
}

/// SetGamingFanSpeed fan IDs.
pub const FAN_CPU: u64 = 1;
pub const FAN_GPU: u64 = 4;

/// Floor for manual fan speed so a slip of the slider cannot stop a fan under load.
pub const FAN_MIN_PERCENT: u8 = 10;

/// GetGamingSysInfo addresses. Value sits in bits 8.. of the output.
pub const SENSOR_CPU_TEMP: u32 = 0x0101;
pub const SENSOR_CPU_RPM: u32 = 0x0201;
pub const SENSOR_GPU_RPM: u32 = 0x0601;
pub const SENSOR_GPU_TEMP: u32 = 0x0A01;

/// GetGamingMiscSetting / SetGamingMiscSetting indices for firmware thermal profiles
/// (PredatorSense v4 / newer Nitro models, matching Linux acer-wmi.c).
pub const MISC_SUPPORTED_PROFILES: u32 = 0x0A;
pub const MISC_PLATFORM_PROFILE: u32 = 0x0B;

/// Firmware thermal profile IDs and labels (bits in GetGamingMiscSetting(0x0A) >> 8).
pub const THERMAL_PROFILES: [(u8, &str); 5] = [
    (0, "Quiet"),
    (1, "Balanced"),
    (4, "Performance"),
    (5, "Turbo"),
    (6, "Eco"),
];

pub fn fan_speed(device: u64, percent: u8) -> u64 {
    device | ((percent.clamp(FAN_MIN_PERCENT, 100) as u64) << 8)
}

pub fn sensor_temp(raw: u64) -> u32 {
    ((raw >> 8) & 0xFF) as u32
}

pub fn sensor_rpm(raw: u64) -> u32 {
    ((raw >> 8) & 0xFFFF) as u32
}

/// Decodes GetGamingMiscSetting(0x0A): status in bits 0-7, supported-profile bitmask in bits 8-15.
pub fn supported_profiles(raw: u64) -> Vec<(u8, &'static str)> {
    if raw & 0xFF != 0 {
        return Vec::new();
    }
    let mask = ((raw >> 8) & 0xFF) as u8;
    THERMAL_PROFILES
        .iter()
        .copied()
        .filter(|&(id, _)| mask & (1u8 << id) != 0)
        .collect()
}

/// Decodes GetGamingMiscSetting(0x0B): status in bits 0-7, active profile ID in bits 8-15.
pub fn active_profile(raw: u64) -> Option<u8> {
    if raw & 0xFF != 0 {
        return None;
    }
    let id = ((raw >> 8) & 0xFF) as u8;
    THERMAL_PROFILES.iter().any(|&(p, _)| p == id).then_some(id)
}

/// SetGamingMiscSetting payload for switching firmware thermal profile.
pub fn set_profile_payload(id: u8) -> u64 {
    (MISC_PLATFORM_PROFILE as u64) | ((id as u64) << 8)
}

/// Keyboard effects, in the order and numbering the firmware uses.
pub const EFFECTS: [&str; 8] = [
    "Static", "Breathing", "Neon", "Wave", "Shifting", "Zoom", "Meteor", "Twinkling",
];

/// 16-byte SetGamingKBBacklight payload: mode, speed (0-9), brightness (0-100), 0,
/// direction (1 right, 2 left), R, G, B, 3, 1, then zeros.
pub fn kb_backlight(mode: u8, speed: u8, brightness: u8, left: bool, rgb: [u8; 3]) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = mode.min(7);
    p[1] = if mode == 0 { 0 } else { speed.clamp(1, 9) };
    p[2] = brightness.min(100);
    p[4] = if left { 2 } else { 1 };
    p[5..8].copy_from_slice(&rgb);
    p[8] = 3;
    p[9] = 1;
    p
}

/// Decodes GetGamingKBBacklight output bytes into (mode, speed, brightness, left, rgb).
pub fn kb_backlight_from(bytes: &[u8]) -> Option<(u8, u8, u8, bool, [u8; 3])> {
    if bytes.len() < 8 {
        return None;
    }
    let mode = bytes[0].min(EFFECTS.len() as u8 - 1);
    let speed = if bytes[1] == 0 { 5 } else { bytes[1].clamp(1, 9) };
    let brightness = bytes[2].min(100);
    let left = bytes[4] == 2;
    let rgb = [bytes[5], bytes[6], bytes[7]];
    Some((mode, speed, brightness, left, rgb))
}

/// SetGamingRgbKb payload for one zone (0..4): zone bit | R<<8 | G<<16 | B<<24.
pub fn zone_color(zone: usize, rgb: [u8; 3]) -> u32 {
    (1u32 << zone) | (rgb[0] as u32) << 8 | (rgb[1] as u32) << 16 | (rgb[2] as u32) << 24
}

/// GetGamingRgbKb output for one zone, back to RGB.
pub fn zone_color_from(raw: u64) -> [u8; 3] {
    [(raw >> 8) as u8, (raw >> 16) as u8, (raw >> 24) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads() {
        assert_eq!(fan_speed(FAN_CPU, 70), 0x4601);
        // The values verified on the AN515-45 (both fans).
        assert_eq!(fan_behavior(true, true, MODE_AUTO), 0x41_0009);
        assert_eq!(fan_behavior(true, true, MODE_MAX), 0x82_0009);
        assert_eq!(fan_behavior(true, true, MODE_CUSTOM), 0xC3_0009);
        assert_eq!(fan_behavior(true, false, MODE_AUTO), 0x1_0001);
        assert_eq!(fan_speed(FAN_GPU, 0), 0x0A04); // clamped to the floor
        assert_eq!(fan_speed(FAN_GPU, 255), 0x6404);
        // Probe values read on the AN515-45: 0x4900 = 73 C, 0xBB800 = 3000 rpm.
        assert_eq!(sensor_temp(0x4900), 73);
        assert_eq!(sensor_rpm(0xBB800), 3000);
        assert_eq!(zone_color(3, [0x11, 0x22, 0x33]), 0x3322_1108);
        assert_eq!(zone_color_from(0xFF00), [0xFF, 0, 0]);
        let kb = kb_backlight(3, 5, 80, true, [1, 2, 3]);
        assert_eq!(kb[..10], [3, 5, 80, 0, 2, 1, 2, 3, 3, 1]);
        assert_eq!(kb_backlight_from(&kb), Some((3, 5, 80, true, [1, 2, 3])));
        assert_eq!(kb_backlight(0, 9, 200, false, [0; 3])[..3], [0, 0, 100]);
    }

    #[test]
    fn thermal_profiles() {
        // AN515-45 returns status=1 (unsupported).
        assert!(supported_profiles(0x1).is_empty());
        // ANV16-71 (Issue #1) returns 0x7300: status=0, mask=0x73 (bits 0, 1, 4, 5, 6).
        let anv16 = supported_profiles(0x7300);
        assert_eq!(
            anv16,
            vec![(0, "Quiet"), (1, "Balanced"), (4, "Performance"), (5, "Turbo"), (6, "Eco")]
        );
        assert_eq!(active_profile(0x0400), Some(4));
        assert_eq!(active_profile(0x0401), None); // non-zero status
        assert_eq!(active_profile(0x0200), None); // unknown ID
        assert_eq!(set_profile_payload(4), 0x040B);
    }
}
