use crate::device::Device;
use crate::packet::Packet;
use crate::types::{adapter_level_watts, AdapterWattage};
use crate::types::{
    BatteryCare, Cluster, CpuBoost, FanMode, FanZone, GpuBoost, LightsAlwaysOn, LogoMode,
    MaxFanSpeedMode, PerfMode, PowerFlag, Toggle,
};

use anyhow::{bail, ensure, Result};

fn _send_command(device: &Device, command: u16, args: &[u8]) -> Result<Packet> {
    let response = device.send(Packet::new(command, args))?;
    ensure!(response.get_args().starts_with(args));
    Ok(response)
}

/// Synapse 4 sleeps this long after every 0x0d02 write (`setThermalFanMode`) so the EC can
/// settle the new mode before the next command.
const PERF_MODE_SETTLE: std::time::Duration = std::time::Duration::from_millis(200);

fn _set_perf_mode(device: &Device, perf_mode: PerfMode, fan_mode: FanMode) -> Result<()> {
    [1, 2].into_iter().try_for_each(|zone| {
        _send_command(
            device,
            0x0d02,
            &[0x01, zone, perf_mode as u8, fan_mode as u8],
        )?;
        std::thread::sleep(PERF_MODE_SETTLE);
        Ok(())
    })
}

fn _set_boost(device: &Device, cluster: Cluster, boost: u8) -> Result<()> {
    let args = &[0x01, cluster as u8, boost];
    let (perf_mode, _) = get_perf_mode(device)?;
    ensure!(
        matches!(perf_mode, PerfMode::Custom | PerfMode::Hyperboost),
        "Performance mode must be {:?} or {:?}",
        PerfMode::Custom,
        PerfMode::Hyperboost
    );
    ensure!(device
        .send(Packet::new(0x0d07, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}

fn _get_boost(device: &Device, cluster: Cluster) -> Result<u8> {
    let response = device.send(Packet::new(0x0d87, &[0, cluster as u8, 0]))?;
    ensure!(response.get_args()[1] == cluster as u8);
    Ok(response.get_args()[2])
}

pub fn set_perf_mode(device: &Device, perf_mode: PerfMode) -> Result<()> {
    _set_perf_mode(device, perf_mode, FanMode::Auto)
}

/// Set the performance mode and fan mode in one write.
///
/// Use this instead of `set_perf_mode` followed by `set_fan_mode(Manual)`: switching to
/// Auto and straight back to Manual leaves the Blade 16 (2023) fans at the Auto speed
/// (about 4400 RPM) while 0x0d81 still reports the manual target.
pub fn set_perf_and_fan_mode(
    device: &Device,
    perf_mode: PerfMode,
    fan_mode: FanMode,
) -> Result<()> {
    _set_perf_mode(device, perf_mode, fan_mode)
}

pub fn get_perf_mode(device: &Device) -> Result<(PerfMode, FanMode)> {
    let [r1, r2]: [Result<(PerfMode, FanMode)>; 2] = [1, 2].map(|zone| {
        let response = device.send(Packet::new(0x0d82, &[0, zone, 0, 0]))?;
        Ok((
            PerfMode::try_from(response.get_args()[2])?,
            FanMode::try_from(response.get_args()[3])?,
        ))
    });

    ensure!(
        r1.is_ok() && r2.is_ok(),
        "Failed to get performance mode and fan mode: r1 = {:?}, r2 = {:?}",
        r1,
        r2
    );

    let r1 = r1?;
    let r2 = r2?;

    //let r1 = r1?;
    ensure!(r1 == r2, "Modes do not match: r1 = {:?}, r2 = {:?}", r1, r2);

    Ok(r1)
}

pub fn set_cpu_boost(device: &Device, boost: CpuBoost) -> Result<()> {
    _set_boost(device, Cluster::Cpu, boost as u8)
}

pub fn set_gpu_boost(device: &Device, boost: GpuBoost) -> Result<()> {
    _set_boost(device, Cluster::Gpu, boost as u8)
}

/// EC temperature sensors in degrees Celsius (0x0d85).
/// On the Blade 16 (2023) sensor 1 tracks the CPU and sensor 2 the GPU.
pub fn get_temperatures(device: &Device) -> Result<Vec<u8>> {
    let response = device.send(Packet::new(0x0d85, &[0; 80]))?;
    let args = response.get_args();
    let count = usize::from(args[0]);
    ensure!(count < args.len(), "Invalid sensor count {}", count);
    Ok(args[1..=count].to_vec())
}

/// Connected and recommended charger power (0x078c).
pub fn get_adapter_wattage(device: &Device) -> Result<AdapterWattage> {
    let response = device.send(Packet::new(0x078c, &[0, 0]))?;
    let args = response.get_args();
    Ok(AdapterWattage {
        connected: adapter_level_watts(args[0]),
        recommended: adapter_level_watts(args[1]),
    })
}

/// Fails when a charger weaker than the recommended one is plugged in: the max profile would
/// then drain the battery while plugged in. Passes when the EC can't report the charger.
pub fn check_adapter_for_max_profile(device: &Device) -> Result<()> {
    if let Ok(adapter) = get_adapter_wattage(device) {
        ensure!(
            !adapter.is_undersized(),
            "Charger is {} W but this laptop needs {} W for the max profile (use --force to apply anyway)",
            adapter.connected.unwrap_or(0),
            adapter.recommended.unwrap_or(0)
        );
    }
    Ok(())
}

pub fn set_max_performance_profile(device: &Device) -> Result<()> {
    // No fixed waits between steps: Device::send waits while the EC reports busy.
    set_perf_and_fan_mode(device, PerfMode::Hyperboost, FanMode::Manual)?;
    set_cpu_boost(device, CpuBoost::High)?;
    set_gpu_boost(device, GpuBoost::High)?;
    set_fan_rpm(device, 5100, false)?;
    set_keyboard_brightness(device, 255)?;
    set_lights_always_on(device, LightsAlwaysOn::Enable)
}

pub fn set_balanced_profile(device: &Device) -> Result<()> {
    set_perf_mode(device, PerfMode::Balanced)?;
    set_keyboard_brightness(device, 255)
}

pub fn set_silent_profile(device: &Device) -> Result<()> {
    set_perf_mode(device, PerfMode::Silent)?;
    set_keyboard_brightness(device, 255)
}

pub fn get_cpu_boost(device: &Device) -> Result<CpuBoost> {
    CpuBoost::try_from(_get_boost(device, Cluster::Cpu)?)
}

pub fn get_gpu_boost(device: &Device) -> Result<GpuBoost> {
    GpuBoost::try_from(_get_boost(device, Cluster::Gpu)?)
}

pub fn set_fan_rpm(device: &Device, rpm: u16, check_mode: bool) -> Result<()> {
    ensure!((0..=5500).contains(&rpm));
    if check_mode {
        ensure!(
            matches!(get_perf_mode(device)?, (_, FanMode::Manual)),
            "Fan mode must be set to {:?}",
            FanMode::Manual
        );
    }
    [FanZone::Zone1, FanZone::Zone2]
        .into_iter()
        .try_for_each(|zone| {
            _send_command(device, 0x0d01, &[0, zone as u8, (rpm / 100) as u8]).map(|_| ())
        })
}

pub fn get_fan_rpm(device: &Device, fan_zone: FanZone) -> Result<u16> {
    let response = device.send(Packet::new(0x0d81, &[0, fan_zone as u8, 0]))?;
    ensure!(response.get_args()[1] == fan_zone as u8);
    Ok(response.get_args()[2] as u16 * 100)
}

pub fn get_fan_actual_rpm(device: &Device, fan_zone: FanZone) -> Result<u16> {
    let response = device.send(Packet::new(0x0d88, &[0, fan_zone as u8, 0]))?;
    ensure!(response.get_args()[1] == fan_zone as u8);
    Ok(response.get_args()[2] as u16 * 100)
}

pub fn send_command(device: &Device, command: u16, args: &[u8]) -> Result<Packet> {
    let response = device.send(Packet::new(command, args))?;
    Ok(response)
}

// 0x070f/0x078f carry a bitfield (see PowerFlag), not a single mode.
// Writes must preserve the bits they don't own.
pub fn get_power_flags(device: &Device) -> Result<u8> {
    Ok(device.send(Packet::new(0x078f, &[0]))?.get_args()[0])
}

pub fn get_power_flag(device: &Device, flag: PowerFlag) -> Result<bool> {
    Ok(get_power_flags(device)? & flag as u8 != 0)
}

pub fn set_power_flag(device: &Device, flag: PowerFlag, on: bool) -> Result<()> {
    let flags = get_power_flags(device)?;
    let flags = if on {
        flags | flag as u8
    } else {
        flags & !(flag as u8)
    };
    _send_command(device, 0x070f, &[flags]).map(|_| ())
}

pub fn set_local_dimming(device: &Device, state: Toggle) -> Result<()> {
    set_power_flag(device, PowerFlag::LocalDimming, state == Toggle::Enable)
}

pub fn get_local_dimming(device: &Device) -> Result<Toggle> {
    Ok(toggle(get_power_flag(device, PowerFlag::LocalDimming)?))
}

pub fn set_charge_full_once(device: &Device, state: Toggle) -> Result<()> {
    set_power_flag(device, PowerFlag::ChargeFullOnce, state == Toggle::Enable)
}

pub fn get_charge_full_once(device: &Device) -> Result<Toggle> {
    Ok(toggle(get_power_flag(device, PowerFlag::ChargeFullOnce)?))
}

fn toggle(on: bool) -> Toggle {
    if on {
        Toggle::Enable
    } else {
        Toggle::Disable
    }
}

pub fn set_max_fan_speed_mode(device: &Device, mode: MaxFanSpeedMode) -> Result<()> {
    ensure!(
        get_perf_mode(device)?.0 == PerfMode::Custom,
        "Performance mode must be {:?}",
        PerfMode::Custom
    );
    set_power_flag(
        device,
        PowerFlag::MaxFanSpeed,
        mode == MaxFanSpeedMode::Enable,
    )
}

pub fn get_max_fan_speed_mode(device: &Device) -> Result<MaxFanSpeedMode> {
    match get_power_flag(device, PowerFlag::MaxFanSpeed)? {
        true => Ok(MaxFanSpeedMode::Enable),
        false => Ok(MaxFanSpeedMode::Disable),
    }
}

pub fn set_fan_mode(device: &Device, mode: FanMode) -> Result<()> {
    _set_perf_mode(device, get_perf_mode(device)?.0, mode)
}

pub fn custom_command(device: &Device, command: u16, args: &[u8]) -> Result<()> {
    let report = Packet::new(command, args);
    println!("Report   {:?}", report);
    let response = device.send(report)?;
    println!("Response {:?}", response);
    Ok(())
}

fn _set_logo_power(device: &Device, mode: LogoMode) -> Result<Packet> {
    match mode {
        LogoMode::Off => _send_command(device, 0x0300, &[1, 4, 0]),
        LogoMode::Static | LogoMode::Breathing => _send_command(device, 0x0300, &[1, 4, 1]),
    }
}

fn _set_logo_mode(device: &Device, mode: LogoMode) -> Result<Packet> {
    match mode {
        LogoMode::Static => _send_command(device, 0x0302, &[1, 4, 0]),
        LogoMode::Breathing => _send_command(device, 0x0302, &[1, 4, 2]),
        _ => bail!("Invalid logo mode"),
    }
}

fn _get_logo_power(device: &Device) -> Result<bool> {
    match device.send(Packet::new(0x0380, &[1, 4, 0]))?.get_args()[2] {
        0 => Ok(false),
        1 => Ok(true),
        _ => bail!("Invalid logo power state"),
    }
}

fn _get_logo_mode(device: &Device) -> Result<LogoMode> {
    match device.send(Packet::new(0x0382, &[1, 4, 0]))?.get_args()[2] {
        0 => Ok(LogoMode::Static),
        2 => Ok(LogoMode::Breathing),
        _ => bail!("Invalid logo power state"),
    }
}

pub fn get_logo_mode(device: &Device) -> Result<LogoMode> {
    let power = _get_logo_power(device)?;
    match power {
        true => _get_logo_mode(device),
        false => Ok(LogoMode::Off),
    }
}

pub fn set_logo_mode(device: &Device, mode: LogoMode) -> Result<()> {
    if mode != LogoMode::Off {
        _set_logo_mode(device, mode)?;
    }
    _set_logo_power(device, mode)?;
    Ok(())
}

pub fn get_keyboard_brightness(device: &Device) -> Result<u8> {
    let response = device.send(Packet::new(0x0383, &[1, 5, 0]))?;
    ensure!(response.get_args()[1] == 5);
    Ok(response.get_args()[2])
}

pub fn set_keyboard_brightness(device: &Device, brightness: u8) -> Result<()> {
    let args = &[1, 5, brightness];
    ensure!(device
        .send(Packet::new(0x0303, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}

pub fn get_lights_always_on(device: &Device) -> Result<LightsAlwaysOn> {
    device.send(Packet::new(0x0084, &[0, 0]))?.get_args()[0].try_into()
}

pub fn set_lights_always_on(device: &Device, lights_always_on: LightsAlwaysOn) -> Result<()> {
    let args = &[lights_always_on as u8, 0];
    ensure!(device
        .send(Packet::new(0x0004, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}

pub fn get_battery_care(device: &Device) -> Result<BatteryCare> {
    device.send(Packet::new(0x0792, &[0]))?.get_args()[0].try_into()
}

pub fn set_battery_care(device: &Device, mode: BatteryCare) -> Result<()> {
    let args = &[mode as u8];
    ensure!(device
        .send(Packet::new(0x0712, args))?
        .get_args()
        .starts_with(args));
    Ok(())
}
