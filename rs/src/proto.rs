//! Acer AcerGamingFunction (root\WMI) payloads for the Nitro AN515-45.
//! Values come from Acer's own software as captured by the Linux driver (Linuwu-Sense,
//! quirk `nitro_sense = 2`) and verified by read-only probes on this machine.

/// Fan mask for this 2-fan model: CPU = bit 0, GPU = bit 3.
pub const FAN_AUTO: u64 = 0x41_0009;
pub const FAN_MAX: u64 = 0x82_0009;
pub const FAN_CUSTOM: u64 = 0xC3_0009;
pub const FAN_CPU: u64 = 1;
pub const FAN_GPU: u64 = 4;

/// Floor for manual fan speed so a slip of the slider cannot stop a fan under load.
pub const FAN_MIN_PERCENT: u8 = 10;

/// GetGamingSysInfo addresses. Value sits in bits 8.. of the output.
pub const SENSOR_CPU_TEMP: u32 = 0x0101;
pub const SENSOR_CPU_RPM: u32 = 0x0201;
pub const SENSOR_GPU_RPM: u32 = 0x0601;
pub const SENSOR_GPU_TEMP: u32 = 0x0A01;

pub fn fan_speed(device: u64, percent: u8) -> u64 {
    device | ((percent.clamp(FAN_MIN_PERCENT, 100) as u64) << 8)
}

pub fn sensor_temp(raw: u64) -> u32 {
    ((raw >> 8) & 0xFF) as u32
}

pub fn sensor_rpm(raw: u64) -> u32 {
    ((raw >> 8) & 0xFFFF) as u32
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
        assert_eq!(fan_speed(FAN_GPU, 0), 0x0A04); // clamped to the floor
        assert_eq!(fan_speed(FAN_GPU, 255), 0x6404);
        // Probe values read on the AN515-45: 0x4900 = 73 C, 0xBB800 = 3000 rpm.
        assert_eq!(sensor_temp(0x4900), 73);
        assert_eq!(sensor_rpm(0xBB800), 3000);
        assert_eq!(zone_color(3, [0x11, 0x22, 0x33]), 0x3322_1108);
        assert_eq!(zone_color_from(0xFF00), [0xFF, 0, 0]);
        let kb = kb_backlight(3, 5, 80, true, [1, 2, 3]);
        assert_eq!(kb[..10], [3, 5, 80, 0, 2, 1, 2, 3, 3, 1]);
        assert_eq!(kb_backlight(0, 9, 200, false, [0; 3])[..3], [0, 0, 100]);
    }
}
