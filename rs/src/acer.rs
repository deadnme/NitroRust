//! Calls into Acer's AcerGamingFunction WMI class. Needs admin rights.

use crate::proto::*;
use wmi::{IWbemClassWrapper, Variant, WMIConnection, WMIError};

pub struct Acer {
    wmi: WMIConnection,
    path: String,
    // Class definition, fetched once: GetMethod on it is local, so each call costs one round trip.
    class: IWbemClassWrapper,
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
        Ok(Self { wmi, path, class })
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

    pub fn telemetry(&self) -> Telemetry {
        let read = |addr| self.get("GetGamingSysInfo", addr).unwrap_or(0);
        Telemetry {
            cpu_temp: sensor_temp(read(SENSOR_CPU_TEMP)),
            gpu_temp: sensor_temp(read(SENSOR_GPU_TEMP)),
            cpu_rpm: sensor_rpm(read(SENSOR_CPU_RPM)),
            gpu_rpm: sensor_rpm(read(SENSOR_GPU_RPM)),
        }
    }

    /// Temperatures only: what the fan curve needs while the window is hidden (2 calls, not 4).
    pub fn temps(&self) -> Telemetry {
        let read = |addr| self.get("GetGamingSysInfo", addr).unwrap_or(0);
        Telemetry {
            cpu_temp: sensor_temp(read(SENSOR_CPU_TEMP)),
            gpu_temp: sensor_temp(read(SENSOR_GPU_TEMP)),
            ..Default::default()
        }
    }

    pub fn fans_auto(&self) -> R<bool> {
        self.set("SetGamingFanBehavior", FAN_AUTO)
    }

    pub fn fans_max(&self) -> R<bool> {
        self.set("SetGamingFanBehavior", FAN_MAX)
    }

    pub fn fans_custom(&self, cpu: u8, gpu: u8) -> R<bool> {
        if !self.set("SetGamingFanBehavior", FAN_CUSTOM)? {
            return Ok(false);
        }
        let speeds = self
            .set("SetGamingFanSpeed", fan_speed(FAN_CPU, cpu))
            .and_then(|a| Ok(a & self.set("SetGamingFanSpeed", fan_speed(FAN_GPU, gpu))?));
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
