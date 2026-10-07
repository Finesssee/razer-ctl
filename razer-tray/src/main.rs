#![windows_subsystem = "windows"]

use anyhow::Error;
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

use librazer::types::{
    AdapterWattage, BatteryCare, CpuBoost, FanMode, GpuBoost, LightsAlwaysOn, LogoMode,
};
use librazer::{command, device};

use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{
    menu::{
        CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem, Submenu,
    },
    TrayIconBuilder, TrayIconEvent,
};

use std::process::Command as procCommand;
use sysinfo::{ProcessExt, Signal, System, SystemExt};

use single_instance::SingleInstance;

#[cfg(target_os = "windows")]
use windows::Win32::Foundation::HANDLE;
#[cfg(target_os = "windows")]
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
#[cfg(target_os = "windows")]
use windows::Win32::System::Threading::{
    GetCurrentProcess, ProcessPowerThrottling, SetPriorityClass, SetProcessInformation,
    IDLE_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION,
    PROCESS_POWER_THROTTLING_EXECUTION_SPEED, PROCESS_POWER_THROTTLING_STATE,
};

const PKG_NAME: &str = env!("CARGO_PKG_NAME");

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum FanSpeed {
    Auto,
    Manual(u16),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_off_bad_read_is_ignored_but_repeated_change_is_kept() {
        let expected = ConfigState::default().ac_state;
        let bad = DeviceState {
            fan_speed: FanSpeed::Manual(0),
            ..expected
        };

        let after_one_bad_read = DeviceState::confirm_read(&expected, bad, || Ok(expected));
        assert_eq!(after_one_bad_read.unwrap(), expected);

        let after_two_bad_reads = DeviceState::confirm_read(&expected, bad, || Ok(bad));
        assert_eq!(after_two_bad_reads.unwrap(), bad);

        let matching = DeviceState::confirm_read(&expected, expected, || {
            panic!("a matching read must not be re-read")
        });
        assert_eq!(matching.unwrap(), expected);
    }

    #[test]
    fn default_ac_profile_is_low_latency_gaming_profile() {
        let config = ConfigState::default();

        assert_eq!(
            config.ac_state.perf_mode,
            PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
        );
        assert_eq!(config.ac_state.fan_speed, FanSpeed::Manual(5500));
        assert_eq!(
            config.ac_state.lights_mode.always_on,
            LightsAlwaysOn::Disable
        );
    }

    #[test]
    fn repairs_battery_mode_saved_as_ac_profile_on_ac_power() {
        let mut config = ConfigState {
            ac_state: DeviceState {
                perf_mode: PerfMode::Battery,
                fan_speed: FanSpeed::Auto,
                ..DeviceState::default()
            },
            battery_state: DeviceState {
                perf_mode: PerfMode::Battery,
                fan_speed: FanSpeed::Auto,
                ..DeviceState::default()
            },
        };

        config.repair_for_startup(true, false);

        assert_eq!(
            config.ac_state.perf_mode,
            PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
        );
        assert_eq!(config.ac_state.fan_speed, FanSpeed::Manual(5500));
        assert_eq!(config.battery_state.perf_mode, PerfMode::Battery);
    }

    #[test]
    fn does_not_repair_battery_mode_when_machine_is_on_battery() {
        let mut config = ConfigState {
            ac_state: DeviceState {
                perf_mode: PerfMode::Battery,
                ..DeviceState::default()
            },
            battery_state: DeviceState::default(),
        };

        config.repair_for_startup(false, false);

        assert_eq!(config.ac_state.perf_mode, PerfMode::Battery);
    }

    #[test]
    fn weak_charger_starts_gaming_profile_instead_of_max() {
        let weak = AdapterWattage {
            connected: Some(100),
            recommended: Some(330),
        };
        let mut config = ConfigState::default();
        config.repair_for_startup(true, true);

        config.avoid_max_on_weak_charger(None);
        assert_eq!(config.ac_state.perf_mode, PerfMode::Max);

        config.avoid_max_on_weak_charger(Some(weak));
        assert_eq!(
            config.ac_state.perf_mode,
            PerfMode::Custom(CpuBoost::Boost, GpuBoost::High)
        );
        assert_eq!(config.ac_state.fan_speed, FanSpeed::Manual(5500));
    }

    #[test]
    fn forced_max_startup_still_requires_explicit_flag() {
        let mut config = ConfigState::default();

        config.repair_for_startup(true, true);

        assert_eq!(config.ac_state.perf_mode, PerfMode::Max);
        assert_eq!(config.ac_state.fan_speed, FanSpeed::Manual(5100));
    }

    #[test]
    fn serializes_custom_ac_profile_without_using_battery_profile() {
        let config = ConfigState::default();

        let serialized = toml::to_string(&config).expect("config should serialize");

        assert!(serialized.contains("[ac_state.perf_mode]"));
        assert!(serialized.contains("Custom = ["));
        assert!(serialized.contains("\"Boost\""));
        assert!(serialized.contains("\"High\""));
        assert!(serialized.contains("Manual = 5500"));
    }

    #[test]
    fn tooltip_drops_trailing_lines_to_fit_windows_limit() {
        let long = (1..=20)
            .map(|i| format!("line {i:02} padding"))
            .collect::<Vec<_>>()
            .join(&String::from(char::from(10)));
        let fitted = fit_tooltip(&long);
        assert!(fitted.encode_utf16().count() <= TOOLTIP_MAX_UNITS);
        assert!(fitted.starts_with("line 01"));
        assert!(long.starts_with(&fitted));
        assert_eq!(fit_tooltip("short"), "short");
    }

    #[test]
    fn detects_lights_only_external_change() {
        let mut active = DeviceState::default();
        active.lights_mode.keyboard_brightness = 128;

        assert!(DeviceState::default().differs_only_lights(&active));

        active.fan_speed = FanSpeed::Manual(5500);
        assert!(!DeviceState::default().differs_only_lights(&active));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
enum PerfMode {
    Max,
    Battery,
    Silent,
    Balanced,
    Performance,
    Hyperboost,
    Custom(CpuBoost, GpuBoost),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct LightsMode {
    logo_mode: LogoMode,
    keyboard_brightness: u8,
    always_on: LightsAlwaysOn,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct FanRpm {
    fan1: u16,
    fan2: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct DeviceState {
    perf_mode: PerfMode,
    lights_mode: LightsMode,
    battery_care: BatteryCare,
    fan_speed: FanSpeed,
}

type Result<T> = std::result::Result<T, Error>;

impl DeviceState {
    /// The EC state to act on after a periodic read. A read that disagrees with the tray is
    /// only trusted if a second read agrees with it; single bad reads (e.g. a fan target of 0)
    /// happen and shouldn't make the tray re-apply its whole profile.
    fn confirm_read(
        expected: &Self,
        first: Self,
        reread: impl FnOnce() -> Result<Self>,
    ) -> Result<Self> {
        if first == *expected {
            return Ok(first);
        }
        let second = reread()?;
        if second == *expected {
            log::info!("ignoring one-off EC read {:?}", first);
        }
        Ok(second)
    }

    fn differs_only_lights(&self, other: &Self) -> bool {
        self.lights_mode != other.lights_mode
            && self.perf_mode == other.perf_mode
            && self.battery_care == other.battery_care
            && self.fan_speed == other.fan_speed
    }

    // The EC sometimes returns a transient bad read (battery care 0x0, or the two perf zones
    // disagreeing mid-switch). Retry the read once before treating it as a device error, so a
    // single bad read no longer triggers a full re-init and state reapply. Reads only; no writes.
    fn read_with_retry(device: &device::Device) -> Result<Self> {
        match Self::read(device) {
            Ok(state) => Ok(state),
            Err(e) => {
                log::warn!("device read failed, retrying once: {:?}", e);
                std::thread::sleep(std::time::Duration::from_millis(300));
                Self::read(device)
            }
        }
    }

    fn read(device: &device::Device) -> Result<Self> {
        let (raw_perf_mode, fan_mode) = command::get_perf_mode(device)?;
        let fan_speed = match fan_mode {
            FanMode::Auto | FanMode::ForceAuto => FanSpeed::Auto,
            FanMode::Manual => {
                let rpm = command::get_fan_rpm(device, librazer::types::FanZone::Zone1)?;
                FanSpeed::Manual(rpm)
            }
        };

        let perf_mode = match raw_perf_mode {
            librazer::types::PerfMode::Battery => PerfMode::Battery,
            librazer::types::PerfMode::Silent => PerfMode::Silent,
            librazer::types::PerfMode::Balanced => PerfMode::Balanced,
            librazer::types::PerfMode::Performance => PerfMode::Performance,
            librazer::types::PerfMode::Hyperboost => {
                let cpu_boost = command::get_cpu_boost(device)?;
                let gpu_boost = command::get_gpu_boost(device)?;
                if cpu_boost == CpuBoost::High
                    && gpu_boost == GpuBoost::High
                    && fan_speed == FanSpeed::Manual(5100)
                {
                    PerfMode::Max
                } else {
                    PerfMode::Hyperboost
                }
            }
            librazer::types::PerfMode::Custom => {
                let cpu_boost = command::get_cpu_boost(device)?;
                let gpu_boost = command::get_gpu_boost(device)?;
                PerfMode::Custom(cpu_boost, gpu_boost)
            }
            mode
            @ (librazer::types::PerfMode::Gaming | librazer::types::PerfMode::BatterySaver) => {
                anyhow::bail!(
                    "Device is in {:?} mode, which razer-tray doesn't manage",
                    mode
                )
            }
        };

        let lights_mode = LightsMode {
            logo_mode: command::get_logo_mode(device)?,
            keyboard_brightness: command::get_keyboard_brightness(device)?,
            always_on: command::get_lights_always_on(device)?,
        };

        let battery_care = command::get_battery_care(device)?;

        Ok(Self {
            perf_mode,
            lights_mode,
            battery_care,
            fan_speed,
        })
    }

    fn apply(&self, device: &device::Device) -> Result<()> {
        use librazer::types::PerfMode as EcPerfMode;
        // Set perf and fan mode in one write; an Auto step before Manual leaves the fans
        // at the Auto speed (see command::set_perf_and_fan_mode).
        let fan_mode = match self.fan_speed {
            FanSpeed::Auto => FanMode::Auto,
            FanSpeed::Manual(_) => FanMode::Manual,
        };
        let set_mode = |mode| command::set_perf_and_fan_mode(device, mode, fan_mode);
        match self.perf_mode {
            PerfMode::Max => command::set_max_performance_profile(device),
            PerfMode::Battery => set_mode(EcPerfMode::Battery),
            PerfMode::Silent => set_mode(EcPerfMode::Silent),
            PerfMode::Balanced => set_mode(EcPerfMode::Balanced),
            PerfMode::Performance => set_mode(EcPerfMode::Performance),
            PerfMode::Hyperboost => set_mode(EcPerfMode::Hyperboost),
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                set_mode(EcPerfMode::Custom)?;
                command::set_cpu_boost(device, cpu_boost)?;
                command::set_gpu_boost(device, gpu_boost)
            }
        }?;

        match self.fan_speed {
            // The max profile always switches the fans to Manual.
            FanSpeed::Auto if self.perf_mode == PerfMode::Max => {
                command::set_fan_mode(device, FanMode::Auto)
            }
            FanSpeed::Auto => Ok(()),
            FanSpeed::Manual(rpm) => command::set_fan_rpm(device, rpm, false),
        }?;

        match self.lights_mode.logo_mode {
            LogoMode::Static => command::set_logo_mode(device, LogoMode::Static),
            LogoMode::Breathing => command::set_logo_mode(device, LogoMode::Breathing),
            LogoMode::Off => command::set_logo_mode(device, LogoMode::Off),
        }?;

        command::set_keyboard_brightness(device, self.lights_mode.keyboard_brightness)?;
        command::set_lights_always_on(device, self.lights_mode.always_on)?;
        command::set_battery_care(device, self.battery_care)
    }

    fn perf_delta(&self, cpu_boost: Option<CpuBoost>, gpu_boost: Option<GpuBoost>) -> Self {
        DeviceState {
            perf_mode: if let PerfMode::Custom(cb, gb) = self.perf_mode {
                PerfMode::Custom(cpu_boost.unwrap_or(cb), gpu_boost.unwrap_or(gb))
            } else {
                PerfMode::Custom(
                    cpu_boost.unwrap_or(CpuBoost::High),
                    gpu_boost.unwrap_or(GpuBoost::High),
                )
            },
            ..*self
        }
    }

    fn max_profile(&self) -> Self {
        Self {
            perf_mode: PerfMode::Max,
            fan_speed: FanSpeed::Manual(5100),
            lights_mode: LightsMode {
                keyboard_brightness: 255,
                always_on: LightsAlwaysOn::Disable,
                ..self.lights_mode
            },
            ..*self
        }
    }

    fn gaming_profile(&self) -> Self {
        Self {
            perf_mode: PerfMode::Custom(CpuBoost::Boost, GpuBoost::High),
            fan_speed: FanSpeed::Manual(5500),
            lights_mode: LightsMode {
                keyboard_brightness: 255,
                always_on: LightsAlwaysOn::Disable,
                ..self.lights_mode
            },
            ..*self
        }
    }

    fn balanced_profile(&self) -> Self {
        Self {
            perf_mode: PerfMode::Balanced,
            fan_speed: FanSpeed::Auto,
            lights_mode: LightsMode {
                keyboard_brightness: 255,
                ..self.lights_mode
            },
            ..*self
        }
    }

    fn silent_profile(&self) -> Self {
        Self {
            perf_mode: PerfMode::Silent,
            fan_speed: FanSpeed::Auto,
            lights_mode: LightsMode {
                keyboard_brightness: 255,
                ..self.lights_mode
            },
            ..*self
        }
    }
}

impl Default for DeviceState {
    fn default() -> Self {
        Self {
            perf_mode: PerfMode::Performance,
            lights_mode: LightsMode {
                logo_mode: LogoMode::Off,
                keyboard_brightness: 0,
                always_on: LightsAlwaysOn::Disable,
            },
            battery_care: BatteryCare::Percent80,
            fan_speed: FanSpeed::Auto,
        }
    }
}

trait DeviceStateDelta<T> {
    fn delta(&self, property: T) -> Self;
}

impl DeviceStateDelta<CpuBoost> for DeviceState {
    fn delta(&self, cpu_boost: CpuBoost) -> Self {
        self.perf_delta(Some(cpu_boost), None)
    }
}

impl DeviceStateDelta<GpuBoost> for DeviceState {
    fn delta(&self, gpu_boost: GpuBoost) -> Self {
        self.perf_delta(None, Some(gpu_boost))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct ConfigState {
    ac_state: DeviceState,
    battery_state: DeviceState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileBucket {
    Ac,
    Battery,
}

impl ProfileBucket {
    fn from_ac_power(ac_power: bool) -> Self {
        if ac_power {
            Self::Ac
        } else {
            Self::Battery
        }
    }
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            ac_state: DeviceState::default().gaming_profile(),
            battery_state: DeviceState {
                perf_mode: PerfMode::Battery,
                ..Default::default()
            },
        }
    }
}

/// The charger, if it is too weak for the max profile. Matches the CLI's guard.
fn undersized_charger(device: &device::Device) -> Option<AdapterWattage> {
    command::get_adapter_wattage(device)
        .ok()
        .filter(|a| a.is_undersized())
}

impl ConfigState {
    /// Start with the gaming profile instead of Max when the charger is too weak for Max.
    fn avoid_max_on_weak_charger(&mut self, weak_charger: Option<AdapterWattage>) {
        if let (PerfMode::Max, Some(adapter)) = (self.ac_state.perf_mode, weak_charger) {
            log::warn!(
                "charger is {:?} W, below the {:?} W the max profile needs; starting with the gaming profile",
                adapter.connected,
                adapter.recommended
            );
            self.ac_state = self.ac_state.gaming_profile();
        }
    }

    fn repair_for_startup(&mut self, ac_power: bool, force_ac_max_profile: bool) {
        if !ac_power {
            return;
        }

        if force_ac_max_profile {
            self.ac_state = self.ac_state.max_profile();
        } else if matches!(self.ac_state.perf_mode, PerfMode::Battery) {
            log::warn!("AC profile was Battery; repairing to gaming profile");
            self.ac_state = self.ac_state.gaming_profile();
        }

        if self.ac_state.lights_mode.always_on == LightsAlwaysOn::Enable {
            log::warn!("AC profile had lights-always-on enabled; disabling to preserve Fn keys");
            self.ac_state.lights_mode.always_on = LightsAlwaysOn::Disable;
        }
    }
}

struct ProgramState {
    device_state: DeviceState,
    ac_state: DeviceState,
    battery_state: DeviceState,
    event_handlers: std::collections::HashMap<String, DeviceState>,
    menu: Menu,
    fan_actual: FanRpm,
    ec_readings: EcReadings,
    ac_power: bool,
}

/// Sensor readings shown in the tooltip. Empty when the EC doesn't support them.
#[derive(Default)]
struct EcReadings {
    temperatures: Vec<u8>,
    adapter: Option<AdapterWattage>,
}

impl EcReadings {
    fn read(device: &device::Device) -> Self {
        Self {
            temperatures: command::get_temperatures(device).unwrap_or_default(),
            adapter: command::get_adapter_wattage(device).ok(),
        }
    }
}

// Windows keeps 128 UTF-16 units of a tray tooltip, including the terminating null.
const TOOLTIP_MAX_UNITS: usize = 127;

/// Drops whole lines from the end until the tooltip fits.
fn fit_tooltip(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    while lines.join("\n").encode_utf16().count() > TOOLTIP_MAX_UNITS && lines.len() > 1 {
        lines.pop();
    }
    lines.join("\n")
}

impl ProgramState {
    fn new(device_state: DeviceState, fan_last: FanRpm) -> Result<Self> {
        let (menu, event_handlers) = Self::create_menu_and_handlers(&device_state)?;
        let fan_actual = fan_last;
        let ac_power = true;
        let ac_state = device_state;
        let battery_state = device_state;
        Ok(Self {
            device_state,
            ac_state,
            battery_state,
            event_handlers,
            menu,
            fan_actual,
            ec_readings: EcReadings::default(),
            ac_power,
        })
    }

    fn create_menu_and_handlers(
        dstate: &DeviceState,
    ) -> Result<(Menu, std::collections::HashMap<String, DeviceState>)> {
        let mut event_handlers = std::collections::HashMap::new();
        let menu = Menu::new();
        // header

        // complete profiles
        let profiles = Submenu::new("Profiles", true);
        let profile_items = [
            (
                "profile:max",
                "Max (Synapse replacement)",
                dstate.max_profile(),
            ),
            ("profile:balanced", "Balanced", dstate.balanced_profile()),
            ("profile:silent", "Silent", dstate.silent_profile()),
        ];
        for (event_id, label, state) in profile_items {
            profiles.append(&MenuItem::with_id(event_id, label, true, None))?;
            event_handlers.insert(event_id.to_string(), state);
        }
        menu.append(&profiles)?;
        menu.append(&PredefinedMenuItem::separator())?;

        // perf
        let perf_modes = Submenu::new("Performance", true);
        // Battery
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Battery),
            "Battery",
            dstate.perf_mode != PerfMode::Battery,
            dstate.perf_mode == PerfMode::Battery,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Battery),
            DeviceState {
                perf_mode: PerfMode::Battery,
                ..*dstate
            },
        );
        // silent
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Silent),
            "Silent",
            dstate.perf_mode != PerfMode::Silent,
            dstate.perf_mode == PerfMode::Silent,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Silent),
            DeviceState {
                perf_mode: PerfMode::Silent,
                ..*dstate
            },
        );
        // balanced
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Balanced),
            "Balanced",
            dstate.perf_mode != PerfMode::Balanced,
            dstate.perf_mode == PerfMode::Balanced,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Balanced),
            DeviceState {
                perf_mode: PerfMode::Balanced,
                ..*dstate
            },
        );
        // performance
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Performance),
            "Performance",
            dstate.perf_mode != PerfMode::Performance,
            dstate.perf_mode == PerfMode::Performance,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Performance),
            DeviceState {
                perf_mode: PerfMode::Performance,
                ..*dstate
            },
        );
        // Hyperboost
        perf_modes.append(&CheckMenuItem::with_id(
            format!("{:?}", PerfMode::Hyperboost),
            "Hyperboost",
            dstate.perf_mode != PerfMode::Hyperboost,
            dstate.perf_mode == PerfMode::Hyperboost,
            None,
        ))?;
        event_handlers.insert(
            format!("{:?}", PerfMode::Hyperboost),
            DeviceState {
                perf_mode: PerfMode::Hyperboost,
                ..*dstate
            },
        );

        // custom
        let cpu_boosts: Vec<CheckMenuItem> = CpuBoost::iter()
            .map(|boost| {
                let event_id = format!("cpu_boost:{:?}", boost);
                event_handlers.insert(event_id.clone(), dstate.delta(boost));
                let checked = matches!(dstate.perf_mode, PerfMode::Custom(b, _) if b == boost);
                CheckMenuItem::with_id(event_id, format!("{:?}", boost), !checked, checked, None)
            })
            .collect();

        let gpu_boosts: Vec<CheckMenuItem> = GpuBoost::iter()
            .map(|boost| {
                let event_id = format!("gpu_boost:{:?}", boost);
                event_handlers.insert(event_id.clone(), dstate.delta(boost));
                let checked = matches!(dstate.perf_mode, PerfMode::Custom(_, b) if b == boost);
                CheckMenuItem::with_id(event_id, format!("{:?}", boost), !checked, checked, None)
            })
            .collect();

        let separator = PredefinedMenuItem::separator();

        perf_modes.append(&Submenu::with_items(
            "Custom",
            true,
            &cpu_boosts
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .chain([&separator as &dyn IsMenuItem])
                .chain(gpu_boosts.iter().map(|i| i as &dyn IsMenuItem))
                .collect::<Vec<_>>(),
        )?)?;

        menu.append(&perf_modes)?;

        // Fan Speed
        menu.append(&PredefinedMenuItem::separator())?;
        let fan_speeds: Vec<CheckMenuItem> = [CheckMenuItem::with_id(
            "fan_speeds:auto",
            "Fan: Auto",
            dstate.fan_speed != FanSpeed::Auto,
            dstate.fan_speed == FanSpeed::Auto,
            None,
        )]
        .into_iter()
        .chain((0..=5500).step_by(500).map(|rpm| {
            let event_id = format!("fan_speeds:{}", rpm);
            event_handlers.insert(
                event_id.clone(),
                DeviceState {
                    fan_speed: FanSpeed::Manual(rpm),
                    ..*dstate
                },
            );
            CheckMenuItem::with_id(
                event_id,
                format!("Fan: {} RPM", rpm),
                dstate.fan_speed != FanSpeed::Manual(rpm),
                dstate.fan_speed == FanSpeed::Manual(rpm),
                None,
            )
        }))
        .collect();
        event_handlers.insert(
            "fan_speeds:auto".to_string(),
            DeviceState {
                fan_speed: FanSpeed::Auto,
                perf_mode: match dstate.perf_mode {
                    PerfMode::Max => PerfMode::Hyperboost,
                    mode => mode,
                },
                ..*dstate
            },
        );

        menu.append(&Submenu::with_items(
            "Fan Speed",
            true,
            &fan_speeds
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;

        // logo
        menu.append(&PredefinedMenuItem::separator())?;
        let modes = LogoMode::iter()
            .map(|mode| {
                let event_id = format!("logo_mode:{:?}", mode);
                event_handlers.insert(
                    event_id.clone(),
                    DeviceState {
                        lights_mode: LightsMode {
                            logo_mode: mode,
                            ..dstate.lights_mode
                        },
                        ..*dstate
                    },
                );
                CheckMenuItem::with_id(
                    event_id,
                    format!("{:?}", mode),
                    dstate.lights_mode.logo_mode != mode,
                    dstate.lights_mode.logo_mode == mode,
                    None,
                )
            })
            .collect::<Vec<_>>();

        menu.append(&Submenu::with_items(
            "Logo",
            true,
            &modes
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;
        menu.append(&PredefinedMenuItem::separator())?;

        // lights always on
        menu.append(&CheckMenuItem::with_id(
            "lights_always_on",
            "Lights always on",
            true,
            dstate.lights_mode.always_on == LightsAlwaysOn::Enable,
            None,
        ))?;
        event_handlers.insert(
            "lights_always_on".to_string(),
            DeviceState {
                lights_mode: LightsMode {
                    always_on: match dstate.lights_mode.always_on {
                        LightsAlwaysOn::Enable => LightsAlwaysOn::Disable,
                        LightsAlwaysOn::Disable => LightsAlwaysOn::Enable,
                    },
                    ..dstate.lights_mode
                },
                ..*dstate
            },
        );

        let brightness_modes: Vec<CheckMenuItem> = (0..=100)
            .step_by(10)
            .map(|brightness| {
                let event_id = format!("brightness:{}", brightness);
                event_handlers.insert(
                    event_id.clone(),
                    DeviceState {
                        lights_mode: LightsMode {
                            keyboard_brightness: brightness / 2 * 5,
                            ..dstate.lights_mode
                        },
                        ..*dstate
                    },
                );
                CheckMenuItem::with_id(
                    event_id,
                    format!("Brightness: {}", brightness),
                    dstate.lights_mode.keyboard_brightness != brightness / 2 * 5,
                    dstate.lights_mode.keyboard_brightness == brightness / 2 * 5,
                    None,
                )
            })
            .collect();

        menu.append(&Submenu::with_items(
            "Brightness",
            true,
            &brightness_modes
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;

        // battery care submenu
        menu.append(&PredefinedMenuItem::separator())?;

        let battery_care_options = [
            (BatteryCare::Percent50, "50%", "battery_care_50"),
            (BatteryCare::Percent55, "55%", "battery_care_55"),
            (BatteryCare::Percent60, "60%", "battery_care_60"),
            (BatteryCare::Percent65, "65%", "battery_care_65"),
            (BatteryCare::Percent70, "70%", "battery_care_70"),
            (BatteryCare::Percent75, "75%", "battery_care_75"),
            (BatteryCare::Percent80, "80%", "battery_care_80"),
            (
                BatteryCare::Disable,
                "Disabled (100%)",
                "battery_care_disable",
            ),
        ];

        let battery_care_items: Vec<CheckMenuItem> = battery_care_options
            .iter()
            .map(|(mode, label, id)| {
                event_handlers.insert(
                    id.to_string(),
                    DeviceState {
                        battery_care: *mode,
                        ..*dstate
                    },
                );
                CheckMenuItem::with_id(id, label, true, dstate.battery_care == *mode, None)
            })
            .collect();

        menu.append(&Submenu::with_items(
            "Battery Care",
            true,
            &battery_care_items
                .iter()
                .map(|i| i as &dyn IsMenuItem)
                .collect::<Vec<_>>(),
        )?)?;

        // gpu task killer
        menu.append(&PredefinedMenuItem::separator())?;
        let terminate_item = MenuItem::with_id(
            "dgpu_terminate_proc",
            "Terminate dGPU processes",
            true,
            None,
        );
        menu.append(&terminate_item)?;
        // footer
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&PredefinedMenuItem::about(None, Some(Self::about())))?;
        menu.append(&PredefinedMenuItem::quit(None))?;

        Ok((menu, event_handlers))
    }

    fn handle_event(&self, event_id: &str) -> Result<DeviceState> {
        let next_state = self.event_handlers.get(event_id).ok_or(anyhow::anyhow!(
            "No event handler found for event_id: {}",
            event_id
        ))?;
        Ok(*next_state)
    }

    fn about() -> tray_icon::menu::AboutMetadata {
        tray_icon::menu::AboutMetadata {
            name: Some(PKG_NAME.into()),
            version: Some(env!("CARGO_PKG_VERSION").into()),
            authors: Some(
                env!("CARGO_PKG_AUTHORS")
                    .split(';')
                    .map(|a| a.trim().to_string())
                    .collect::<Vec<_>>(),
            ),
            website: Some(format!(
                "{}\nLog: {}",
                env!("CARGO_PKG_HOMEPAGE"),
                get_logging_file_path().display()
            )),
            comments: Some(env!("CARGO_PKG_DESCRIPTION").into()),
            ..Default::default()
        }
    }

    fn get_next_perf_mode(&self) -> DeviceState {
        DeviceState {
            perf_mode: match self.device_state.perf_mode {
                PerfMode::Max => PerfMode::Battery,
                PerfMode::Battery => PerfMode::Silent,
                PerfMode::Silent => PerfMode::Balanced,
                PerfMode::Balanced => PerfMode::Performance,
                PerfMode::Performance => PerfMode::Hyperboost,
                PerfMode::Hyperboost => PerfMode::Custom(CpuBoost::High, GpuBoost::High),
                PerfMode::Custom(..) => PerfMode::Battery,
            },
            ..self.device_state
        }
    }

    fn tooltip(&self) -> Result<String> {
        use std::fmt::Write;
        let mut info = String::new();
        let mut status = String::new();

        match self.device_state.perf_mode {
            PerfMode::Max => writeln!(&mut info, "Max")?,
            PerfMode::Battery => writeln!(&mut info, "Battery")?,
            PerfMode::Silent => writeln!(&mut info, "Silent")?,
            PerfMode::Balanced => writeln!(&mut info, "Balanced")?,
            PerfMode::Performance => writeln!(&mut info, "Performance")?,
            PerfMode::Hyperboost => writeln!(&mut info, "Hyperboost")?,
            PerfMode::Custom(cpu_boost, gpu_boost) => {
                writeln!(&mut info, "Custom",)?;
                writeln!(&mut info, "CPU: {:?}", cpu_boost)?;
                writeln!(&mut info, "GPU: {:?}", gpu_boost)?;
            }
        }
        match self.ec_readings.temperatures.as_slice() {
            [] => {}
            // Sensor 1 is the CPU and sensor 2 the GPU on the Blade 16 (2023).
            [cpu, gpu] => writeln!(&mut info, "CPU {}°C  GPU {}°C", cpu, gpu)?,
            temps => writeln!(&mut info, "Temps {:?} °C", temps)?,
        }
        if let Some(adapter) = self.ec_readings.adapter.filter(|a| a.is_undersized()) {
            writeln!(
                &mut info,
                "Charger {} W, needs {} W",
                adapter.connected.unwrap_or(0),
                adapter.recommended.unwrap_or(0)
            )?;
        }
        match self.device_state.fan_speed {
            FanSpeed::Auto => writeln!(&mut info, "Fan Auto")?,
            FanSpeed::Manual(rpm) => writeln!(&mut info, "Fan {:?} RPM", rpm)?,
        }

        writeln!(
            &mut info,
            "Fan actual : {:?}, {:?} PRM",
            self.fan_actual.fan1, self.fan_actual.fan2,
        )?;

        writeln!(
            &mut info,
            "Logo: {:?}",
            self.device_state.lights_mode.logo_mode
        )?;

        if self.device_state.lights_mode.keyboard_brightness > 0 {
            writeln!(
                &mut info,
                "🔆: {:?}",
                self.device_state.lights_mode.keyboard_brightness
            )?;
        }

        if self.device_state.lights_mode.always_on == LightsAlwaysOn::Enable {
            status.push('💡');
        }

        // Battery care with percentage
        match self.device_state.battery_care {
            BatteryCare::Disable => {} // No indicator for disabled
            _ => {
                writeln!(
                    &mut info,
                    "🔋 Battery Care: {}%",
                    self.device_state.battery_care.to_percent()
                )?;
            }
        }

        Ok(fit_tooltip((info.to_string() + &status).trim_end()))
    }

    fn icon(&self) -> tray_icon::Icon {
        let razer_red = include_bytes!("../icons/razer-red.png");
        let razer_blue = include_bytes!("../icons/razer-blue.png");
        let razer_brown = include_bytes!("../icons/razer-brown.png");
        let razer_yellow = include_bytes!("../icons/razer-yellow.png");
        let razer_green = include_bytes!("../icons/razer-green.png");
        let razer_violet = include_bytes!("../icons/razer-violet.png");

        let image = match self.device_state.perf_mode {
            PerfMode::Max => image::load_from_memory(razer_violet),
            PerfMode::Battery => image::load_from_memory(razer_blue),
            PerfMode::Silent => image::load_from_memory(razer_yellow),
            PerfMode::Balanced => image::load_from_memory(razer_green),
            PerfMode::Performance => image::load_from_memory(razer_red),
            PerfMode::Hyperboost => image::load_from_memory(razer_violet),
            PerfMode::Custom(_, _) => image::load_from_memory(razer_brown),
        };

        let (icon_rgba, icon_width, icon_height) = {
            let image = image.expect("Failed to open icon").into_rgba8();
            let (width, height) = image.dimensions();
            let rgba = image.into_raw();
            (rgba, width, height)
        };
        tray_icon::Icon::from_rgba(icon_rgba, icon_width, icon_height).expect("Failed to open icon")
    }

    fn update(
        &mut self,
        tray_icon: &mut tray_icon::TrayIcon,
        new_device_state: DeviceState,
        device: &device::Device,
        bucket: ProfileBucket,
    ) -> Result<()> {
        self.device_state = new_device_state;
        self.device_state.apply(device)?;
        (self.menu, self.event_handlers) = Self::create_menu_and_handlers(&self.device_state)?;
        self.fan_actual = get_fan_rpm(device)?;
        self.ec_readings = EcReadings::read(device);
        if self.device_state.perf_mode == PerfMode::Max
            && self.ec_readings.adapter.is_some_and(|a| a.is_undersized())
        {
            log::warn!(
                "max profile applied with an undersized charger: {:?}",
                self.ec_readings.adapter
            );
        }
        match bucket {
            ProfileBucket::Ac => self.ac_state = self.device_state,
            ProfileBucket::Battery => self.battery_state = self.device_state,
        }
        confy::store(
            PKG_NAME,
            None,
            ConfigState {
                ac_state: self.ac_state,
                battery_state: self.battery_state,
            },
        )?;
        tray_icon.set_icon(Some(self.icon()))?;
        tray_icon.set_tooltip(Some(self.tooltip()?))?;
        tray_icon.set_menu(Some(Box::new(self.menu.clone())));

        log::info!("state updated to {:?}", new_device_state);
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn get_power_state() -> Result<bool> {
    let mut ac_power: bool = true;
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        match GetSystemPowerStatus(&mut status) {
            Ok(()) => match status.ACLineStatus {
                0 => ac_power = false,
                _ => ac_power = true,
            },
            Err(e) => {
                eprintln!("Failed to get power status: {:?}", e);
            }
        }
    }
    Ok(ac_power)
}

#[cfg(target_os = "linux")]
fn get_power_state() -> Result<bool> {
    // Try AC adapter first
    if let Ok(online) = std::fs::read_to_string("/sys/class/power_supply/AC/online")
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/AC0/online"))
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/ACAD/online"))
    {
        return Ok(online.trim() == "1");
    }

    // Fallback: check battery status
    if let Ok(status) = std::fs::read_to_string("/sys/class/power_supply/BAT0/status")
        .or_else(|_| std::fs::read_to_string("/sys/class/power_supply/BAT1/status"))
    {
        let status = status.trim();
        return Ok(status == "Charging" || status == "Full" || status == "Not charging");
    }

    // Default to AC power if we can't detect
    log::warn!("Could not detect power state, assuming AC power");
    Ok(true)
}

fn get_fan_rpm(device: &device::Device) -> Result<FanRpm> {
    let fan_actual = FanRpm {
        fan1: command::get_fan_actual_rpm(device, librazer::types::FanZone::Zone1)?,
        fan2: command::get_fan_actual_rpm(device, librazer::types::FanZone::Zone2)?,
    };
    //log::info!("fans updated to {:?}", fan_actual);
    Ok(fan_actual)
}

#[cfg(target_os = "windows")]
fn gpu_taskkill() -> Result<()> {
    use std::os::windows::process::CommandExt;
    let whitelist: &[&str] = &["explorer.exe", "Insufficient Permissions"];

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let output = procCommand::new("nvidia-smi")
        .args(["--query-compute-apps=name,pid", "--format=csv,noheader"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .expect("Failed to execute nvidia-smi");

    if !output.status.success() {
        log::info!("nvidia-smi command failed or no GPU processes found");
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines();

    let mut pids_to_kill = Vec::new();

    for line in lines {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            continue;
        }

        let name = parts[0];
        let pid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if whitelist.contains(&name) {
            log::info!("Skipping whitelisted process: {} ({})", pid, name);
        } else {
            pids_to_kill.push((pid, name.to_string()));
        }
    }

    if pids_to_kill.is_empty() {
        log::info!("No GPU-using processes to kill.");
        return Ok(());
    }

    let mut sys = System::new_all();
    sys.refresh_processes();

    for (pid, name) in pids_to_kill {
        if let Some(process) = sys.process(sysinfo::Pid::from(pid as usize)) {
            log::info!("Attempting to kill process {} ({})", pid, name);
            if process.kill_with(Signal::Kill).unwrap_or(false) {
                log::info!("Successfully killed PID {}", pid);
            } else {
                log::info!("Failed to kill PID {}", pid);
            }
        } else {
            log::info!("Process with PID {} not found", pid);
        }
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn gpu_taskkill() -> Result<()> {
    // dGPU process termination for Linux
    let output = procCommand::new("nvidia-smi")
        .args(&["--query-compute-apps=name,pid", "--format=csv,noheader"])
        .output();

    if output.is_err() {
        log::info!("nvidia-smi not found or no GPU processes");
        return Ok(());
    }

    let output = output?;
    if !output.status.success() {
        return Ok(());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut system = System::new_all();
    system.refresh_all();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.split(',').map(|s| s.trim()).collect();
        if parts.len() != 2 {
            continue;
        }

        let pid: usize = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };

        if let Some(process) = system.process(sysinfo::Pid::from(pid)) {
            log::info!("Terminating GPU process: {} (PID: {})", parts[0], pid);
            process.kill_with(Signal::Term);
        }
    }

    Ok(())
}

fn get_logging_file_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!("{}.log", PKG_NAME))
}

fn init_logging_to_file() -> Result<()> {
    use log4rs::append::rolling_file::policy::compound::{
        roll::delete::DeleteRoller, trigger::size::SizeTrigger, CompoundPolicy,
    };
    let policy = CompoundPolicy::new(
        Box::new(SizeTrigger::new(50 << 20)),
        Box::new(DeleteRoller::new()),
    );

    let logfile = log4rs::append::rolling_file::RollingFileAppender::builder()
        .encoder(Box::new(log4rs::encode::pattern::PatternEncoder::new(
            "{h({d(%Y-%m-%d %H:%M:%S)(local)} - {l}: {m}{n})}",
        )))
        .build(get_logging_file_path(), Box::new(policy))?;

    let config = log4rs::config::Config::builder()
        .appender(log4rs::config::Appender::builder().build("logfile", Box::new(logfile)))
        .build(
            log4rs::config::Root::builder()
                .appender("logfile")
                .build(log::LevelFilter::Trace),
        )?;

    log4rs::init_config(config)?;
    Ok(())
}

fn init(
    tray_icon: &mut tray_icon::TrayIcon,
    device: &device::Device,
    force_ac_max_profile: bool,
) -> Result<ProgramState> {
    log::info!(
        "loading config file {}",
        confy::get_configuration_file_path(PKG_NAME, None)?.display()
    );
    let mut config: ConfigState = confy::load(PKG_NAME, None).unwrap_or_default();
    let fan_actual = get_fan_rpm(device)?;
    let ac_power = get_power_state()?;
    config.repair_for_startup(ac_power, force_ac_max_profile);
    if ac_power {
        config.avoid_max_on_weak_charger(undersized_charger(device));
    }

    let mut state = ProgramState::new(config.ac_state, fan_actual)?;
    state.ac_power = ac_power;
    state.ac_state = config.ac_state;
    state.battery_state = config.battery_state;
    if !state.ac_power {
        state.device_state = state.battery_state
    }
    state.update(
        tray_icon,
        state.device_state,
        device,
        ProfileBucket::from_ac_power(ac_power),
    )?;
    Ok(state)
}

#[cfg(target_os = "windows")]
fn efficiency_mode() {
    unsafe {
        let handle: HANDLE = GetCurrentProcess();

        let _ = SetPriorityClass(handle, IDLE_PRIORITY_CLASS);

        let power_throttling = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        };
        let _ = SetProcessInformation(
            handle,
            ProcessPowerThrottling,
            &power_throttling as *const _ as *mut _,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );
    }
}

fn main() -> Result<()> {
    // Log startup errors too; as a windows_subsystem app there is no console to see them.
    let result = run();
    if let Err(e) = &result {
        log::error!("exiting with error: {:?}", e);
    }
    result
}

fn run() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // Initialize GTK for tray icon on Linux
        gtk::init().map_err(|_| anyhow::anyhow!("Failed to initialize GTK"))?;
    }

    #[cfg(target_os = "windows")]
    efficiency_mode();

    // Create a named mutex (unique string for your app)
    let instance = SingleInstance::new("razer-tray").unwrap();
    if !instance.is_single() {
        println!("Another instance is already running. Exiting.");
        return Ok(());
    }

    init_logging_to_file()?;
    log::info!("{0} starting {1} {0}", "==".repeat(20), PKG_NAME);

    // Panics otherwise vanish (no console), so the tray just disappears with no trace.
    std::panic::set_hook(Box::new(|info| {
        log::error!("panic, exiting: {}", info);
    }));

    let device = match device::Device::detect() {
        Ok(d) => {
            log::info!(
                "detected device: {} (0x{:04X})",
                d.info().name,
                d.info().pid
            );
            d
        }
        Err(e) => {
            log::error!("{:?}", e);
            native_dialog::MessageDialog::new()
                .set_type(native_dialog::MessageType::Error)
                .set_text(format!("{:?}", e).as_str())
                .show_alert()?;
            return Err(e);
        }
    };

    let mut tray_icon = TrayIconBuilder::new().build()?;

    let force_ac_max_profile = std::env::args().any(|arg| arg == "--profile=max" || arg == "--max");
    let mut state: ProgramState = init(&mut tray_icon, &device, force_ac_max_profile)?;

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let event_loop = EventLoopBuilder::new().build();

    let mut last_device_state_check_timestamp = std::time::Instant::now();

    // loop through the default start up sequence to initialise the device.
    for element in device.info().init_cmds {
        command::send_command(&device, *element, &[0, 0, 0, 0])?;
    }

    event_loop.run(move |event, _, control_flow| {
        if let tao::event::Event::LoopDestroyed = event {
            log::info!("exiting: event loop closed (Quit menu or Windows ended the app)");
            return;
        }

        let now = std::time::Instant::now();
        *control_flow = ControlFlow::WaitUntil(now + std::time::Duration::from_millis(1000));

        if let Err(e) = (|| -> Result<()> {
            state.ac_power = get_power_state()?;
            let profile_bucket = ProfileBucket::from_ac_power(state.ac_power);

            if let Ok(event) = menu_channel.try_recv() {
                log::info!("Menu Event {:?}", event.id);
                if event.id == MenuId("dgpu_terminate_proc".to_string()) {
                    log::info!("match event id");
                    gpu_taskkill()?;
                } else {
                    let new_device_state = state.handle_event(event.id.as_ref())?;
                    log::info!("new_device_state 1 {:?}", new_device_state);
                    let weak_charger = (new_device_state.perf_mode == PerfMode::Max
                        && state.device_state.perf_mode != PerfMode::Max)
                        .then(|| undersized_charger(&device))
                        .flatten();
                    if let Some(adapter) = weak_charger {
                        // The tooltip already shows the charger warning.
                        log::warn!(
                            "not switching to the max profile: charger is {:?} W, needs {:?} W",
                            adapter.connected,
                            adapter.recommended
                        );
                    } else {
                        state.update(&mut tray_icon, new_device_state, &device, profile_bucket)?;
                    }
                }
            }

            if matches!(tray_channel.try_recv(), Ok(event) if event.click_type == tray_icon::ClickType::Left) {
                let new_device_state = state.get_next_perf_mode();
                log::info!("new_device_state 2 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device, profile_bucket)?;
            }

            if state.ac_power && state.device_state != state.ac_state {
                let new_device_state = state.ac_state;
                log::info!("new_device_state 3 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device, profile_bucket)?;
            } else if !state.ac_power && state.device_state != state.battery_state {
                let new_device_state = state.battery_state;
                log::info!("new_device_state 3 {:?}", new_device_state);
                state.update(&mut tray_icon, new_device_state, &device, profile_bucket)?;
            }

            if now > last_device_state_check_timestamp + std::time::Duration::from_secs(10)
            {
                last_device_state_check_timestamp = now;
                state.fan_actual =  get_fan_rpm(&device)?;
                state.ec_readings = EcReadings::read(&device);
                let active_device_state = DeviceState::confirm_read(
                    &state.device_state,
                    DeviceState::read_with_retry(&device)?,
                    || {
                        std::thread::sleep(std::time::Duration::from_millis(300));
                        DeviceState::read_with_retry(&device)
                    },
                )?;
                if active_device_state != state.device_state {
                    if state.device_state.differs_only_lights(&active_device_state) {
                        log::info!(
                            "adopting external lights change {:?}",
                            active_device_state.lights_mode
                        );
                        state.update(&mut tray_icon, active_device_state, &device, profile_bucket)?;
                    } else {
                        log::warn!("reapplying tray state after external EC change {:?},",
                                  active_device_state);
                        state.update(&mut tray_icon, state.device_state, &device, profile_bucket)?;
                    }
               } else {
                    tray_icon.set_tooltip(Some(state.tooltip()?))?;
               }
            }

            Ok(())
        })() {
            log::error!("trying to recover from: {:?}", e);
            match init(&mut tray_icon, &device, force_ac_max_profile) {
                Ok(new_state) => {
                    state = new_state;
                },
                Err(e) => {
                    log::error!("failed to recover: {:?}", e);
                    *control_flow = ControlFlow::WaitUntil(now + std::time::Duration::from_secs(5));
                }
            }
        }
    })
}
