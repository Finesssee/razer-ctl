//! Readable device report shared by `info` and `status`.
//!
//! Values use the same spelling as the commands that set them (`custom`, `boost`, `static`), and a
//! failed read shows up on its own line instead of hiding the rest of the report.

use anyhow::Result;
use clap::ValueEnum;
use librazer::command;
use librazer::device::Device;
use librazer::types::{
    AdapterWattage, CpuBoost, FanMode, FanZone, GpuBoost, LightsAlwaysOn, MaxFanSpeedMode,
    PerfMode, Toggle,
};
use std::collections::BTreeMap;
use std::process::Command;
use sysinfo::{ProcessExt, System, SystemExt};

const LABEL_WIDTH: usize = 13;

pub fn row(label: &str, value: impl std::fmt::Display) {
    println!("{:<width$} {}", label, value, width = LABEL_WIDTH);
}

/// Prints the device state; `with_system` adds NVIDIA power and Razer app conflicts.
pub fn print(device: &Device, with_system: bool) {
    let info = &device.info;
    let has = |feature: &str| info.features.contains(&feature);

    // `manual` mode has no descriptor, so it only knows the PID.
    match info.model_number_prefix {
        "Unknown" => println!("Unknown device  PID {:#06x}", info.pid),
        model => println!("{}  {}  PID {:#06x}", info.name, model, info.pid),
    }
    println!();

    let perf = command::get_perf_mode(device);
    if has("perf") {
        row("Performance", performance(device, &perf));
    }
    if has("fan") {
        row("Fan", fan(device, &perf));
    }
    row(
        "Temperature",
        value(command::get_temperatures(device), |t| temperatures(&t)),
    );
    row(
        "Charger",
        value(command::get_adapter_wattage(device), charger),
    );
    if has("battery-care") {
        row("Battery care", battery_care(device));
    }

    let mut lighting = Vec::new();
    if has("kbd-backlight") {
        lighting.push(value(command::get_keyboard_brightness(device), |b| {
            format!("keyboard {}/255", b)
        }));
    }
    if has("lid-logo") {
        lighting.push(value(command::get_logo_mode(device), |m| {
            format!("logo {}", name(m))
        }));
    }
    if has("lights-always-on") {
        lighting.push(value(command::get_lights_always_on(device), |m| {
            format!("always-on {}", on_off(m == LightsAlwaysOn::Enable))
        }));
    }
    if !lighting.is_empty() {
        row("Lighting", lighting.join(", "));
    }
    if has("local-dimming") {
        row(
            "Display",
            value(command::get_local_dimming(device), |t| {
                format!("local dimming {}", on_off(t == Toggle::Enable))
            }),
        );
    }

    if with_system {
        println!();
        row("GPU power", gpu_power());
        row("Conflicts", razer_conflicts());
    }
}

pub fn battery_care(device: &Device) -> String {
    let limit = value(command::get_battery_care(device), |care| {
        match care.to_percent() {
            100 => "off, charges to 100%".to_string(),
            percent => format!("{}% limit", percent),
        }
    });
    match command::get_charge_full_once(device) {
        Ok(Toggle::Enable) => format!("{}, charging to full once", limit),
        _ => limit,
    }
}

fn performance(device: &Device, perf: &Result<(PerfMode, FanMode)>) -> String {
    match perf {
        Ok((mode @ (PerfMode::Custom | PerfMode::Hyperboost), _)) => format!(
            "{} (CPU {}, GPU {})",
            name(*mode),
            value(command::get_cpu_boost(device), name::<CpuBoost>),
            value(command::get_gpu_boost(device), name::<GpuBoost>),
        ),
        Ok((mode, _)) => name(*mode),
        Err(e) => failed(e),
    }
}

fn fan(device: &Device, perf: &Result<(PerfMode, FanMode)>) -> String {
    let actual = value(command::get_fan_actual_rpm(device, FanZone::Zone1), |rpm| {
        format!("{} RPM", rpm)
    });
    let mode = match perf {
        Ok((_, FanMode::Manual)) => value(command::get_fan_rpm(device, FanZone::Zone1), |rpm| {
            format!("manual, target {} RPM", rpm)
        }),
        Ok((_, FanMode::Auto)) => "auto".to_string(),
        Ok((_, FanMode::ForceAuto)) => "forced auto".to_string(),
        Err(_) => "mode unknown".to_string(),
    };
    let max_speed = matches!(
        command::get_max_fan_speed_mode(device),
        Ok(MaxFanSpeedMode::Enable)
    );
    format!(
        "{} ({}{})",
        actual,
        mode,
        if max_speed { ", max fan speed on" } else { "" }
    )
}

/// Two EC sensors are CPU then GPU on the Blade 16 (2023); other layouts are listed by index.
fn temperatures(temps: &[u8]) -> String {
    match temps {
        [cpu, gpu] => format!("CPU {} °C, GPU {} °C", cpu, gpu),
        [] => "no sensors reported".to_string(),
        _ => temps
            .iter()
            .enumerate()
            .map(|(i, t)| format!("sensor {} {} °C", i + 1, t))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

fn charger(adapter: AdapterWattage) -> String {
    let watts = |w: Option<u16>| match w {
        Some(w) => format!("{} W", w),
        None => "unknown".to_string(),
    };
    let connected = match adapter.connected {
        Some(0) => "not connected".to_string(),
        w => watts(w),
    };
    let warning = if adapter.is_undersized() {
        "; weaker than recommended, max profile is blocked"
    } else {
        ""
    };
    format!(
        "{} (recommended {}){}",
        connected,
        watts(adapter.recommended),
        warning
    )
}

fn gpu_power() -> String {
    match Command::new("nvidia-smi")
        .args(["-q", "-d", "POWER"])
        .output()
    {
        Ok(output) if output.status.success() => {
            gpu_power_text(&String::from_utf8_lossy(&output.stdout))
                .unwrap_or_else(|| "no readings from nvidia-smi".to_string())
        }
        Ok(output) => format!(
            "unavailable ({})",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(e) => format!("unavailable ({})", e),
    }
}

/// Picks the draw and limits out of `nvidia-smi -q -d POWER`, skipping fields reported as N/A.
fn gpu_power_text(stdout: &str) -> Option<String> {
    let field = |key: &str| {
        stdout
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with(key))
            .filter_map(|line| line.split_once(':'))
            .map(|(_, v)| v.trim().to_string())
            .find(|v| v != "N/A")
    };
    let draw = field("Instantaneous Power Draw").map(|w| format!("{} draw", w));
    let limit = field("Current Power Limit").map(|w| format!("limit {}", w));
    let range = [
        field("Default Power Limit").map(|w| format!("default {}", w)),
        field("Max Power Limit").map(|w| format!("max {}", w)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();

    let mut parts: Vec<String> = [draw, limit].into_iter().flatten().collect();
    if !range.is_empty() {
        let range = format!("({})", range.join(", "));
        match parts.last_mut() {
            Some(last) => *last = format!("{} {}", last, range),
            None => parts.push(range),
        }
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

fn razer_conflicts() -> String {
    let mut system = System::new_all();
    system.refresh_processes();

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for process in system.processes().values() {
        let name = process.name();
        if name.contains("RazerAppEngine")
            || name.contains("Synapse")
            || name.contains("RazerCentral")
            || name.contains("Cortex")
        {
            *counts.entry(name.to_string()).or_default() += 1;
        }
    }
    conflicts_text(&counts)
}

fn conflicts_text(counts: &BTreeMap<String, usize>) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    let names = counts
        .iter()
        .map(|(name, n)| match n {
            1 => name.clone(),
            n => format!("{} ({} processes)", name, n),
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}; these can override razer-ctl settings", names)
}

/// The CLI spelling of a value, so the report reads the same as the commands that set it.
fn name<T: ValueEnum>(value: T) -> String {
    value
        .to_possible_value()
        .map(|v| v.get_name().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

fn failed(e: &anyhow::Error) -> String {
    format!("read failed ({})", e.root_cause())
}

fn value<T>(result: Result<T>, format: impl FnOnce(T) -> String) -> String {
    match result {
        Ok(v) => format(v),
        Err(e) => failed(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA_SMI: &str = "
==============NVSMI LOG==============
GPU 00000000:01:00.0
    GPU Power Readings
        Average Power Draw                : 36.90 W
        Instantaneous Power Draw          : 37.30 W
        Current Power Limit               : 170.23 W
        Requested Power Limit             : 170.23 W
        Default Power Limit               : 115.00 W
        Min Power Limit                   : 1.00 W
        Max Power Limit                   : 175.00 W
    Module Power Readings
        Instantaneous Power Draw          : N/A
        Current Power Limit               : N/A
";

    #[test]
    fn gpu_power_reads_draw_and_limits_on_one_line() {
        assert_eq!(
            gpu_power_text(NVIDIA_SMI).as_deref(),
            Some("37.30 W draw, limit 170.23 W (default 115.00 W, max 175.00 W)")
        );
    }

    #[test]
    fn gpu_power_skips_fields_reported_as_na() {
        let stdout = "Instantaneous Power Draw : N/A\nMax Power Limit : 175.00 W\n";
        assert_eq!(gpu_power_text(stdout).as_deref(), Some("(max 175.00 W)"));
        assert_eq!(gpu_power_text("Instantaneous Power Draw : N/A\n"), None);
    }

    #[test]
    fn charger_explains_a_weak_or_missing_adapter() {
        let adapter = |connected, recommended| AdapterWattage {
            connected: Some(connected),
            recommended: Some(recommended),
        };
        assert_eq!(charger(adapter(330, 330)), "330 W (recommended 330 W)");
        assert_eq!(
            charger(adapter(230, 330)),
            "230 W (recommended 330 W); weaker than recommended, max profile is blocked"
        );
        assert_eq!(
            charger(adapter(0, 330)),
            "not connected (recommended 330 W)"
        );
    }

    #[test]
    fn values_use_cli_spelling() {
        assert_eq!(name(PerfMode::Hyperboost), "hyperboost");
        assert_eq!(name(CpuBoost::SynapseOverclock), "synapse-overclock");
        assert_eq!(temperatures(&[76, 56]), "CPU 76 °C, GPU 56 °C");
    }

    #[test]
    fn conflicts_group_repeated_processes() {
        let mut counts = BTreeMap::new();
        assert_eq!(conflicts_text(&counts), "none");
        counts.insert("RazerAppEngine.exe".to_string(), 6);
        counts.insert("RazerCentralService.exe".to_string(), 1);
        assert_eq!(
            conflicts_text(&counts),
            "RazerAppEngine.exe (6 processes), RazerCentralService.exe; \
             these can override razer-ctl settings"
        );
    }
}
