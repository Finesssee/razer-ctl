use anyhow::ensure;
use serde::{Deserialize, Serialize};
use serde_big_array::BigArray;
use std::sync::atomic::{AtomicU8, Ordering};

/// Packet is the structure of the packet that is sent to the Razer HID device and received back.
/// Source https://github.com/Razer-Linux/razer-laptop-control-no-dkms/blob/main/razer_control_gui/src/device.rs.
#[repr(C)]
#[derive(Serialize, Deserialize, Debug)]
pub struct Packet {
    status: u8,
    id: u8,
    remaining_packets: u16,
    protocol_type: u8,
    data_size: u8,
    command_class: u8,
    command_id: u8,
    #[serde(with = "BigArray")]
    args: [u8; 80],
    crc: u8,
    reserved: u8,
}

const STATUS_NEW: u8 = 0x00;
const STATUS_BUSY: u8 = 0x01;
const STATUS_SUCCESSFUL: u8 = 0x02;
const STATUS_FAILURE: u8 = 0x03;
const STATUS_TIMEOUT: u8 = 0x04;

/// Status names as listed by Synapse 4 (RZ_STATUS_DESC).
fn status_name(status: u8) -> &'static str {
    match status {
        0x00 => "new command",
        0x01 => "busy",
        0x02 => "successful",
        0x03 => "failure",
        0x04 => "no response (timeout)",
        0x05 => "command not supported",
        0x06 => "profile not supported",
        0x07 => "target ID not supported",
        0x08 => "grab control",
        0x09 => "not supported in current device mode",
        0x0a => "broadcast",
        0x0b => "unknown command",
        _ => "unrecognized status",
    }
}

/// Synapse 4 numbers transactions 0..=30 and wraps, instead of using random IDs.
fn next_transaction_id() -> u8 {
    static NEXT_ID: AtomicU8 = AtomicU8::new(0);
    NEXT_ID.fetch_add(1, Ordering::Relaxed) % 31
}

/// What to do with a reply read back for a report.
pub(crate) enum Reply {
    /// The device completed the command.
    Done,
    /// The device is still working on the command; read again.
    Busy,
    /// The reply belongs to another command, or the device dropped or failed it; send again.
    Resend(String),
    /// The device rejected the command; sending again will not help.
    Rejected(String),
}

impl Packet {
    pub fn new(command: u16, args: &[u8]) -> Packet {
        let mut args_buffer = [0x00; 80];
        args_buffer[..args.len()].copy_from_slice(args);

        let mut packet = Packet {
            status: STATUS_NEW,
            id: next_transaction_id(),
            remaining_packets: 0x0000,
            protocol_type: 0x00,
            data_size: args.len() as u8,
            command_class: (command >> 8) as u8,
            command_id: (command & 0xff) as u8,
            args: args_buffer,
            crc: 0x00,
            reserved: 0x00,
        };
        packet.crc = packet.calc_crc();
        packet
    }

    pub fn set_args(&mut self, args: &[u8]) {
        self.args[..args.len()].copy_from_slice(args)
    }

    pub fn get_args(&self) -> &[u8] {
        &self.args
    }

    fn calc_crc(&self) -> u8 {
        // XOR of bytes 2..87 (status through args, excluding crc and reserved)
        // per the Razer USB HID protocol
        let bytes: Vec<u8> = bincode::serialize(self).unwrap();
        bytes[2..88].iter().fold(0u8, |acc, &b| acc ^ b)
    }

    pub fn command(&self) -> u16 {
        u16::from(self.command_class) << 8 | u16::from(self.command_id)
    }

    /// Classify this packet as the reply to `report`, following Synapse 4's protocol-25 rules.
    pub(crate) fn classify_reply(&self, report: &Packet) -> Reply {
        if (self.id, self.command_class, self.command_id)
            != (report.id, report.command_class, report.command_id)
        {
            return Reply::Resend(format!(
                "reply {:04x} id {} does not match report {:04x} id {}",
                self.command(),
                self.id,
                report.command(),
                report.id
            ));
        }

        match self.status {
            // Synapse 4 ignores remaining_packets; some replies (e.g. 0x078f, 0x0d85) set it.
            STATUS_SUCCESSFUL => Reply::Done,
            STATUS_BUSY => Reply::Busy,
            STATUS_NEW | STATUS_FAILURE | STATUS_TIMEOUT => Reply::Resend(format!(
                "device reported {} (0x{:02x})",
                status_name(self.status),
                self.status
            )),
            status => Reply::Rejected(format!(
                "device reported {} (0x{:02x})",
                status_name(status),
                status
            )),
        }
    }
}

impl From<&Packet> for Vec<u8> {
    fn from(packet: &Packet) -> Vec<u8> {
        bincode::serialize(packet).unwrap()
    }
}

impl TryFrom<&[u8]> for Packet {
    type Error = anyhow::Error;

    fn try_from(data: &[u8]) -> Result<Self, Self::Error> {
        ensure!(
            data.len() == std::mem::size_of::<Packet>(),
            "Invalid raw data size"
        );

        Ok(bincode::deserialize::<Packet>(data)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply_to(report: &Packet, status: u8) -> Packet {
        let mut bytes = Vec::<u8>::from(report);
        bytes[0] = status;
        Packet::try_from(bytes.as_slice()).unwrap()
    }

    #[test]
    fn transaction_ids_stay_in_synapse_range() {
        assert!((0..300).all(|_| next_transaction_id() <= 30));
    }

    #[test]
    fn crc_is_xor_of_bytes_2_to_87() {
        let packet = Packet::new(0x0d02, &[0x01, 0x01, 0x04, 0x00]);
        let bytes = Vec::<u8>::from(&packet);
        assert_eq!(bytes[88], bytes[2..88].iter().fold(0, |acc, &b| acc ^ b));
    }

    #[test]
    fn classifies_replies_like_synapse() {
        let report = Packet::new(0x0d82, &[0, 1, 0, 0]);
        assert!(matches!(
            reply_to(&report, STATUS_SUCCESSFUL).classify_reply(&report),
            Reply::Done
        ));
        assert!(matches!(
            reply_to(&report, STATUS_BUSY).classify_reply(&report),
            Reply::Busy
        ));
        for status in [STATUS_NEW, STATUS_FAILURE, STATUS_TIMEOUT] {
            assert!(matches!(
                reply_to(&report, status).classify_reply(&report),
                Reply::Resend(_)
            ));
        }
        for status in [0x05, 0x09, 0x0b, 0x42] {
            assert!(matches!(
                reply_to(&report, status).classify_reply(&report),
                Reply::Rejected(_)
            ));
        }
    }

    #[test]
    fn reply_for_another_transaction_is_resent() {
        let report = Packet::new(0x0d82, &[0, 1, 0, 0]);
        let other = Packet::new(0x0d82, &[0, 1, 0, 0]);
        let stale = reply_to(&other, STATUS_SUCCESSFUL);
        assert!(matches!(stale.classify_reply(&report), Reply::Resend(_)));
    }
}
