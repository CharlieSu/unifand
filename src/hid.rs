use anyhow::{bail, Context, Result};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

pub const VENDOR_ID: u16 = 0x0cf2;
pub const PRODUCT_ID: u16 = 0xa105;
pub const REPORT_ID: u8 = 224;
pub const REPORT_LEN: usize = 65;

/// SL V2 duty -> wire byte, per uni-sync (fan range 250-2000 rpm).
pub fn speed_byte(duty: u8) -> u8 {
    let d = duty.min(100) as f64;
    ((250.0 + 17.5 * d) as usize / 20) as u8
}

pub fn rgb_sync_off_buf() -> [u8; 7] {
    [REPORT_ID, 16, 97, 0, 0, 0, 0]
}

/// Put one channel (1-based) in manual mode (motherboard-PWM sync off).
pub fn manual_mode_buf(channel: u8) -> [u8; 4] {
    [REPORT_ID, 16, 98, 0x10 << (channel - 1)]
}

pub fn speed_buf(channel: u8, duty: u8) -> [u8; 4] {
    [REPORT_ID, 31 + channel, 0, speed_byte(duty)]
}

/// RPM for a 1-based channel out of a 65-byte input report.
pub fn parse_rpm(report: &[u8], channel: u8) -> Option<u16> {
    if !(1..=4).contains(&channel) {
        return None;
    }
    let i = 2 + (channel as usize - 1) * 2;
    if report.len() < i + 2 {
        return None;
    }
    Some(u16::from_be_bytes([report[i], report[i + 1]]))
}

/// linux HIDIOCGINPUT(len): _IOC(READ|WRITE, 'H', 0x0A, len)
pub fn hidiocginput_code(len: usize) -> libc::c_ulong {
    ((3u64 << 30) | ((len as u64) << 16) | ((b'H' as u64) << 8) | 0x0A) as libc::c_ulong
}

/// Does a /sys/class/hidraw/*/device/uevent blob describe our hub?
pub fn uevent_matches(uevent: &str) -> bool {
    let needle = format!("{:04X}:0000{:04X}", VENDOR_ID, PRODUCT_ID);
    uevent
        .lines()
        .any(|l| l.starts_with("HID_ID=") && l.contains(&needle))
}

pub fn find_hidraw_in(base: &Path) -> Result<PathBuf> {
    for entry in std::fs::read_dir(base).with_context(|| format!("reading {}", base.display()))? {
        let entry = entry?;
        let uevent = entry.path().join("device").join("uevent");
        let Ok(contents) = std::fs::read_to_string(&uevent) else {
            continue;
        };
        if uevent_matches(&contents) {
            return Ok(PathBuf::from("/dev").join(entry.file_name()));
        }
    }
    bail!(
        "no hidraw device matching {:04x}:{:04x} under {}",
        VENDOR_ID,
        PRODUCT_ID,
        base.display()
    );
}

pub fn find_hidraw() -> Result<PathBuf> {
    find_hidraw_in(Path::new("/sys/class/hidraw"))
}

pub struct Hub {
    file: File,
}

impl Hub {
    pub fn open(dev_path: &Path) -> Result<Hub> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dev_path)
            .with_context(|| format!("opening {}", dev_path.display()))?;
        Ok(Hub { file })
    }

    /// One-time setup: RGB-header sync off, then each channel to manual mode.
    /// The hub races on rapid writes; both reference implementations pace
    /// init writes at 200 ms.
    pub fn init(&mut self, channels: &[u8]) -> Result<()> {
        self.file
            .write_all(&rgb_sync_off_buf())
            .context("rgb sync off")?;
        sleep(Duration::from_millis(200));
        for &c in channels {
            self.file
                .write_all(&manual_mode_buf(c))
                .with_context(|| format!("manual mode ch{}", c))?;
            sleep(Duration::from_millis(200));
        }
        Ok(())
    }

    pub fn set_duty(&mut self, channel: u8, duty: u8) -> Result<()> {
        self.file
            .write_all(&speed_buf(channel, duty))
            .with_context(|| format!("set duty ch{} {}%", channel, duty))
    }

    /// Set multi-color effect on one channel's LEDs: start packet → multi_color_packet → commit.
    /// The hub races on rapid writes; pacing: 10ms between packets.
    pub fn set_effect(
        &mut self,
        channel: u8,
        num_fans: u8,
        colors: &[(u8, u8, u8)],
        mode: u8,
        speed: u8,
        brightness: u8,
    ) -> Result<()> {
        use crate::rgb;
        self.file
            .write_all(&rgb::start_packet(channel, num_fans))
            .with_context(|| format!("rgb start ch{}", channel))?;
        sleep(Duration::from_millis(10));
        self.file
            .write_all(&rgb::multi_color_packet(channel, colors))
            .with_context(|| format!("rgb multi_color ch{}", channel))?;
        sleep(Duration::from_millis(10));
        self.file
            .write_all(&rgb::commit_packet(channel, mode, speed, brightness))
            .with_context(|| format!("rgb commit ch{}", channel))?;
        sleep(Duration::from_millis(10));
        Ok(())
    }

    /// Re-send commit packet only: used for spike-validated escalation (e.g., speed adjustment).
    pub fn set_effect_speed(
        &mut self,
        channel: u8,
        mode: u8,
        speed: u8,
        brightness: u8,
    ) -> Result<()> {
        use crate::rgb;
        self.file
            .write_all(&rgb::commit_packet(channel, mode, speed, brightness))
            .with_context(|| format!("rgb commit ch{}", channel))?;
        Ok(())
    }

    /// Set a uniform static color on one channel's LEDs. Same pacing rule as
    /// init(): the hub races on rapid writes.
    pub fn set_rgb(
        &mut self,
        channel: u8,
        num_fans: u8,
        color: (u8, u8, u8),
        brightness: u8,
    ) -> Result<()> {
        use crate::rgb;
        self.set_effect(
            channel,
            num_fans,
            &[color],
            rgb::MODE_STATIC,
            rgb::SPEEDS[0],
            brightness,
        )
    }

    pub fn read_rpm(&self, channel: u8) -> Result<u16> {
        let mut buf = [0u8; REPORT_LEN];
        buf[0] = REPORT_ID;
        let rc = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                hidiocginput_code(REPORT_LEN),
                buf.as_mut_ptr(),
            )
        };
        if rc < 0 {
            bail!("HIDIOCGINPUT failed: {}", std::io::Error::last_os_error());
        }
        parse_rpm(&buf, channel).context("rpm parse")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_byte_matches_slv2_formula() {
        assert_eq!(speed_byte(0), 12); // (250 + 0) / 20
        assert_eq!(speed_byte(50), 56); // (250 + 875) / 20 = 1125/20
        assert_eq!(speed_byte(100), 100); // (250 + 1750) / 20
        assert_eq!(speed_byte(150), 100); // clamped to 100%
    }

    #[test]
    fn buffers_match_reference_implementations() {
        assert_eq!(rgb_sync_off_buf(), [224, 16, 97, 0, 0, 0, 0]);
        assert_eq!(manual_mode_buf(1), [224, 16, 98, 0x10]);
        assert_eq!(manual_mode_buf(3), [224, 16, 98, 0x40]);
        assert_eq!(speed_buf(1, 100), [224, 32, 0, 100]);
        assert_eq!(speed_buf(2, 0), [224, 33, 0, 12]);
    }

    #[test]
    fn rpm_parses_big_endian_at_channel_offset() {
        let mut report = [0u8; REPORT_LEN];
        report[0] = REPORT_ID;
        report[2] = 0x07;
        report[3] = 0xD0; // ch1 = 2000
        report[4] = 0x03;
        report[5] = 0xE8; // ch2 = 1000
        assert_eq!(parse_rpm(&report, 1), Some(2000));
        assert_eq!(parse_rpm(&report, 2), Some(1000));
        assert_eq!(parse_rpm(&report, 5), None); // bad channel
        assert_eq!(parse_rpm(&report[..4], 2), None); // short report
    }

    #[test]
    fn ioctl_code_matches_hidiocginput_65() {
        // _IOC(READ|WRITE, 'H', 0x0A, 65) computed by hand
        assert_eq!(hidiocginput_code(REPORT_LEN), 0xC041480A);
    }

    #[test]
    fn uevent_matching() {
        assert!(uevent_matches("HID_ID=0003:00000CF2:0000A105\nHID_NAME=x"));
        assert!(!uevent_matches("HID_ID=0003:00000CF2:0000A102\n"));
        assert!(!uevent_matches("HID_ID=0003:0000046D:0000C52B\n"));
    }

    #[test]
    fn discovery_finds_matching_hidraw_node() {
        let base = tempfile::tempdir().unwrap();
        for (node, hid_id) in [
            ("hidraw3", "HID_ID=0003:0000046D:0000C52B\n"),
            ("hidraw10", "HID_ID=0003:00000CF2:0000A105\n"),
        ] {
            let d = base.path().join(node).join("device");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("uevent"), hid_id).unwrap();
        }
        let p = find_hidraw_in(base.path()).unwrap();
        assert_eq!(p, std::path::PathBuf::from("/dev/hidraw10"));
    }

    #[test]
    fn discovery_errors_when_absent() {
        let base = tempfile::tempdir().unwrap();
        assert!(find_hidraw_in(base.path()).is_err());
    }
}
