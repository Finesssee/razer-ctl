use crate::descriptor::{Descriptor, SUPPORTED};
use crate::packet::{Packet, Reply};

use anyhow::{bail, Context, Result};
use std::fs::File;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

pub struct Device {
    device: hidapi::HidDevice,
    pub info: Descriptor,
    // Held for a whole transaction, so threads in this process never interleave packets.
    last_send: Mutex<Option<Instant>>,
    // Locked for a whole transaction, so other razer-ctl processes (tray, CLI) don't either.
    // The EC keeps one reply buffer, so interleaved sends overwrite each other's replies.
    process_lock: Option<File>,
}

/// Holds the cross-process lock until dropped. The OS also releases it if the process dies.
struct ProcessLock<'a>(&'a File);

impl<'a> ProcessLock<'a> {
    fn acquire(file: &'a File) -> Result<Self> {
        file.lock()
            .context("Failed to lock the razer-ctl HID lock file")?;
        Ok(ProcessLock(file))
    }
}

impl Drop for ProcessLock<'_> {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn open_process_lock() -> Option<File> {
    File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join("razer-ctl-hid.lock"))
        .ok()
}

// Read the model id and clip to conform with https://mysupport.razer.com/app/answers/detail/a_id/5481
#[cfg(target_os = "windows")]
fn read_device_model() -> Result<String> {
    let hklm = winreg::RegKey::predef(winreg::enums::HKEY_LOCAL_MACHINE);
    let bios = hklm.open_subkey("HARDWARE\\DESCRIPTION\\System\\BIOS")?;
    let system_sku: String = bios.get_value("SystemSKU")?;
    Ok(system_sku.chars().take(10).collect())
}

#[cfg(target_os = "linux")]
fn read_device_model() -> Result<String> {
    let sku = fs::read_to_string("/sys/devices/virtual/dmi/id/product_sku")
        .map(|s| s.trim().to_string())
        .map_err(|e| anyhow::anyhow!("Failed to read product SKU: {}", e))?;

    debug!("Linux product SKU: {}", sku);

    if sku.starts_with("RZ") {
        Ok(sku.chars().take(10).collect())
    } else {
        anyhow::bail!("Invalid SKU format: {}", sku)
    }
}

impl Device {
    const RAZER_VID: u16 = 0x1532;

    pub fn info(&self) -> &Descriptor {
        &self.info
    }

    pub fn new(descriptor: Descriptor) -> Result<Device> {
        let api = hidapi::HidApi::new().context("Failed to create hid api")?;

        // Razer protocol uses 90-byte packets + 1 byte report ID = 91 bytes.
        // The probe must match this size so that on Windows, HidD_SetFeature
        // only succeeds on the interface with the correct FeatureReportByteLength.
        // A smaller probe (e.g. 2 bytes) would pass on keyboard interfaces
        // that have small feature reports, causing the wrong interface to be selected.
        let probe_report = vec![0u8; 1 + std::mem::size_of::<Packet>()];

        // there are multiple devices with the same pid, pick first that support feature report
        for info in api.device_list().filter(|info| {
            (info.vendor_id(), info.product_id()) == (Device::RAZER_VID, descriptor.pid)
        }) {
            let path = info.path();
            let device = match api.open_path(path) {
                Ok(device) => device,
                Err(_) => continue,
            };
            if device.send_feature_report(&probe_report).is_ok() {
                return Ok(Device {
                    device,
                    info: descriptor.clone(),
                    last_send: Mutex::new(None),
                    process_lock: open_process_lock(),
                });
            }
        }
        anyhow::bail!("Failed to open device {:?}", descriptor)
    }

    /// Send a report and wait for its reply.
    ///
    /// Timing and retries follow Synapse 4's protocol-25 transport (rzDevice25.sendCommand):
    /// keep writes at least 5 ms apart, read the reply 5 ms after a write, keep reading every
    /// 5 ms (up to 10 reads) while the device is busy, and write again (up to 20 writes) when
    /// the reply belongs to another command or the device dropped or failed it.
    pub fn send(&self, report: Packet) -> Result<Packet> {
        const MAX_WRITES: usize = 20;
        const MAX_READS: usize = 10;
        const SETTLE: Duration = Duration::from_millis(5);

        let mut last_send = self.last_send.lock().unwrap_or_else(|e| e.into_inner());
        let _process_lock = self
            .process_lock
            .as_ref()
            .map(ProcessLock::acquire)
            .transpose()?;

        // extra byte for report id
        let request: Vec<u8> = std::iter::once(0_u8)
            .chain(Vec::<u8>::from(&report))
            .collect();
        let mut response_buf: Vec<u8> = vec![0x00; request.len()];
        let mut problem = String::from("no reply");

        for _ in 0..MAX_WRITES {
            if let Some(sent_at) = *last_send {
                let elapsed = sent_at.elapsed();
                if elapsed < SETTLE {
                    thread::sleep(SETTLE - elapsed);
                }
            }

            self.device
                .send_feature_report(&request)
                .context("Failed to send feature report")?;
            *last_send = Some(Instant::now());

            for _ in 0..MAX_READS {
                thread::sleep(SETTLE);

                let response_size = self.device.get_feature_report(&mut response_buf)?;
                if response_size != response_buf.len() {
                    problem = format!("response size {} != {}", response_size, response_buf.len());
                    break;
                }

                // skip report id byte
                let response = <&[u8] as TryInto<Packet>>::try_into(&response_buf[1..])?;
                match response.classify_reply(&report) {
                    Reply::Done => return Ok(response),
                    Reply::Busy => problem = "device stayed busy".into(),
                    Reply::Resend(reason) => {
                        problem = reason;
                        break;
                    }
                    Reply::Rejected(reason) => {
                        bail!("Command {:04x} rejected: {}", report.command(), reason)
                    }
                }
            }
        }

        bail!(
            "Command {:04x} failed after {} attempts: {}",
            report.command(),
            MAX_WRITES,
            problem
        )
    }

    pub fn enumerate() -> Result<(Vec<u16>, String)> {
        let razer_pid_list: Vec<_> = hidapi::HidApi::new()?
            .device_list()
            .filter(|info| info.vendor_id() == Device::RAZER_VID)
            .map(|info| info.product_id())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        if razer_pid_list.is_empty() {
            anyhow::bail!("No Razer devices found")
        }

        match read_device_model() {
            Ok(model) if model.starts_with("RZ09-") => Ok((razer_pid_list, model)),
            Ok(model) => anyhow::bail!("Detected model but it's not a Razer laptop: {}", model),
            Err(e) => anyhow::bail!("Failed to detect model: {}", e),
        }
    }

    pub fn detect() -> Result<Device> {
        let (pid_list, model_number_prefix) = Device::enumerate()?;

        match SUPPORTED
            .iter()
            .find(|supported| model_number_prefix == supported.model_number_prefix)
        {
            Some(supported) => Device::new(supported.clone()),
            None => anyhow::bail!(
                "Model {} with PIDs {:0>4x?} is not supported",
                model_number_prefix,
                pid_list
            ),
        }
    }
}
