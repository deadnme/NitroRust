//! Calls into Acer's AcerGamingFunction WMI class. Needs admin rights.

use crate::proto::*;
use wmi::{IWbemClassWrapper, Variant, WMIConnection, WMIError};

pub struct Acer {
    wmi: WMIConnection,
    path: String,
    // Class definition, fetched once: GetMethod on it is local, so each call costs one round trip.
    class: IWbemClassWrapper,
    /// What this laptop has; set from the probe right after open(). Defaults = AN515-45.
    pub caps: Option<crate::caps::Caps>,
}

#[derive(serde::Deserialize)]
#[allow(non_snake_case)]
struct Instance {
    __Path: String,
}

#[derive(Default, Clone, Copy)]
pub struct Telemetry {
    pub cpu_temp: u32,
    pub gpu_temp: u32,
    pub cpu_rpm: u32,
    pub gpu_rpm: u32,
}

type R<T> = Result<T, WMIError>;

impl Acer {
    pub fn open() -> R<Self> {
        let wmi = WMIConnection::with_namespace_path("ROOT\\WMI")?;
        let found: Vec<Instance> = wmi.raw_query("SELECT __Path FROM AcerGamingFunction")?;
        let path = found
            .into_iter()
            .next()
            .ok_or_else(|| WMIError::ResultEmpty)?
            .__Path;
        let class = wmi.get_object("AcerGamingFunction")?;
        Ok(Self { wmi, path, class, caps: None })
    }

    /// Read-only questions about what the firmware supports. Never changes anything.
    pub fn probe(&self) -> crate::caps::Probe {
        let get = |m: &str, v: u32| self.get(m, v).ok();
        crate::caps::Probe {
            sys_info: get("GetGamingSysInfo", 0x0000),
            fan_behavior: get("GetGamingFanBehavior", 0x1),
            kb_backlight: self.kb_backlight_state().ok(),
            rgb_zone: get("GetGamingRgbKb", 0x1),
            misc_profiles: get("GetGamingMiscSetting", 0x0A),
        }
    }

    /// GetGamingKBBacklight: (gmReturn, gmOutput bytes). It takes no input.
    fn kb_backlight_state(&self) -> R<(u8, Vec<u8>)> {
        let out = self.wmi.exec_method(&self.path, "GetGamingKBBacklight", None)?.ok_or(WMIError::ResultEmpty)?;
        let ret = match out.get_property("gmReturn")? {
            Variant::UI1(v) => v,
            Variant::I4(v) => v as u8,
            _ => 0xFF,
        };
        let bytes = match out.get_property("gmOutput")? {
            Variant::Array(a) => a
                .into_iter()
                .map(|v| match v {
                    Variant::UI1(b) => b,
                    Variant::I4(b) => b as u8,
                    _ => 0,
                })
                .collect(),
            _ => Vec::new(),
        };
        Ok((ret, bytes))
    }

    /// (cpu fan, gpu fan) the firmware is told about.
    fn fans(&self) -> (bool, bool) {
        self.caps.as_ref().map_or((true, true), |c| (c.cpu_fan, c.gpu_fan))
    }

    /// Runs one gaming method. `input` goes in as gmInput; gmOutput comes back as a number.
    fn call(&self, method: &str, input: Variant) -> R<u64> {
        let params = self
            .class
            .get_method(method)?
            .ok_or(WMIError::ResultEmpty)?
            .spawn_instance()?;
        params.put_property("gmInput", input)?;
        let out = self.wmi.exec_method(&self.path, method, Some(&params))?;
        let value = match out {
            Some(o) => o.get_property("gmOutput")?,
            None => return Ok(0),
        };
        Ok(match value {
            Variant::UI8(v) => v,
            Variant::UI4(v) => v as u64,
            Variant::I4(v) => v as u32 as u64,
            Variant::String(s) => s.parse().unwrap_or(0),
            _ => 0,
        })
    }

    /// Set* methods answer 0 for success.
    fn set(&self, method: &str, input: u64) -> R<bool> {
        Ok(self.call(method, Variant::UI8(input))? == 0)
    }

    fn get(&self, method: &str, input: u32) -> R<u64> {
        self.call(method, Variant::UI4(input))
    }

    /// Reads one sensor if this laptop has it; 0 otherwise.
    fn sensor(&self, has: bool, addr: u32) -> u64 {
        // Bits 0-7 are a status: anything but 0 is an error reply, not a reading.
        if has { self.get("GetGamingSysInfo", addr).ok().filter(|v| v & 0xFF == 0).unwrap_or(0) } else { 0 }
    }

    pub fn telemetry(&self) -> Telemetry {
        let c = self.caps.clone().unwrap_or_else(|| crate::caps::Caps::from_probe("", &Default::default(), false));
        Telemetry {
            cpu_temp: sensor_temp(self.sensor(c.cpu_temp, SENSOR_CPU_TEMP)),
            gpu_temp: sensor_temp(self.sensor(c.gpu_temp, SENSOR_GPU_TEMP)),
            cpu_rpm: sensor_rpm(self.sensor(c.cpu_rpm, SENSOR_CPU_RPM)),
            gpu_rpm: sensor_rpm(self.sensor(c.gpu_rpm, SENSOR_GPU_RPM)),
        }
    }

    pub fn fans_auto(&self) -> R<bool> {
        let (c, g) = self.fans();
        self.set("SetGamingFanBehavior", fan_behavior(c, g, MODE_AUTO))
    }

    pub fn fans_max(&self) -> R<bool> {
        let (c, g) = self.fans();
        self.set("SetGamingFanBehavior", fan_behavior(c, g, MODE_MAX))
    }

    pub fn fans_custom(&self, cpu: u8, gpu: u8) -> R<bool> {
        let (has_cpu, has_gpu) = self.fans();
        if !self.set("SetGamingFanBehavior", fan_behavior(has_cpu, has_gpu, MODE_CUSTOM))? {
            return Ok(false);
        }
        let speed = |has: bool, id, pct| if has { self.set("SetGamingFanSpeed", fan_speed(id, pct)) } else { Ok(true) };
        let speeds = speed(has_cpu, FAN_CPU, cpu).and_then(|a| Ok(a & speed(has_gpu, FAN_GPU, gpu)?));
        if !matches!(speeds, Ok(true)) {
            // Never leave the fans in custom mode at an unknown speed.
            let _ = self.fans_auto();
        }
        speeds
    }

    /// Current zone colours, read from the firmware.
    pub fn zone_colors(&self) -> [[u8; 3]; 4] {
        let mut c = [[255, 0, 0]; 4];
        for (z, slot) in c.iter_mut().enumerate() {
            if let Ok(raw) = self.get("GetGamingRgbKb", 1 << z) {
                *slot = zone_color_from(raw);
            }
        }
        c
    }

    pub fn keyboard_effect(&self, mode: u8, speed: u8, brightness: u8, left: bool, rgb: [u8; 3]) -> R<bool> {
        let bytes = kb_backlight(mode, speed, brightness, left, rgb);
        let arr = Variant::Array(bytes.iter().map(|&b| Variant::UI1(b)).collect());
        Ok(self.call("SetGamingKBBacklight", arr)? == 0)
    }

    /// Static mode with one colour per zone.
    pub fn keyboard_zones(&self, brightness: u8, zones: &[[u8; 3]; 4]) -> R<bool> {
        let mut ok = self.keyboard_effect(0, 0, brightness, false, [0; 3])?;
        for (z, rgb) in zones.iter().enumerate() {
            ok &= self.call("SetGamingRgbKb", Variant::UI4(zone_color(z, *rgb)))? == 0;
        }
        Ok(ok)
    }
}
