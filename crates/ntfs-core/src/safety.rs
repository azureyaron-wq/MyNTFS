use crate::attribute::{Attribute, AttributeType};
use crate::boot::BootSector;
use crate::device::BlockDevice;
use crate::error::{Error, Result};
use crate::mft::MftRecord;
use crate::volume::NtfsVolume;

/// Policy applied when a caller asks to open a volume writable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePolicy {
    /// Never mutate. Default.
    ReadOnly,
    /// Mutate only if every safety gate passes.
    ReadWriteIfSafe,
    /// Caller has explicitly accepted remaining risk after gates.
    ForceWrite,
}

#[derive(Debug, Clone, Default)]
pub struct SafetyReport {
    /// Set only after a successful NTFS boot-sector parse. Write paths require this.
    pub verified: bool,
    pub dirty: bool,
    pub hibernated: bool,
    pub bitlocker: bool,
    pub efs_present: bool,
    pub paragon_claimed: bool,
    pub reasons: Vec<String>,
}

impl SafetyReport {
    pub fn writable(&self, policy: WritePolicy) -> bool {
        match policy {
            WritePolicy::ReadOnly => false,
            WritePolicy::ForceWrite => self.verified && !self.bitlocker,
            WritePolicy::ReadWriteIfSafe => {
                self.verified
                    && !self.dirty
                    && !self.hibernated
                    && !self.bitlocker
                    && !self.efs_present
            }
        }
    }
}

/// Probe a raw device/image before handing it to the write engine.
pub struct VolumeProbe;

impl VolumeProbe {
    pub fn probe(dev: &dyn BlockDevice) -> Result<SafetyReport> {
        let mut report = SafetyReport::default();
        let mut boot = vec![0u8; 512];
        if let Err(e) = dev.read_at(0, &mut boot) {
            report.reasons.push(format!("boot read failed: {e}"));
            return Ok(report);
        }
        match BootSector::parse(&boot) {
            Err(Error::Safety(msg)) if msg.contains("BitLocker") => {
                report.bitlocker = true;
                report.reasons.push(msg);
                return Ok(report);
            }
            Err(e) => {
                report.reasons.push(format!("not NTFS: {e}"));
                return Ok(report);
            }
            Ok(boot) => {
                report.verified = true;
                if let Ok(vol) = NtfsVolume::from_boot(dev, boot) {
                    inspect_volume(dev, &vol, &mut report)?;
                }
            }
        }
        Ok(report)
    }
}

fn inspect_volume(
    dev: &dyn BlockDevice,
    vol: &NtfsVolume,
    report: &mut SafetyReport,
) -> Result<()> {
    // $Volume record is MFT #3. $VOLUME_INFORMATION flags bit 0 = dirty.
    if let Ok(rec) = vol.read_record(dev, 3) {
        if let Ok(attrs) = rec.attributes() {
            for a in attrs {
                if a.header().ty == AttributeType::VOLUME_INFORMATION {
                    if let Attribute::Resident { value, .. } = a {
                        if value.len() >= 8 {
                            let flags = u16::from_le_bytes(value[6..8].try_into().unwrap());
                            if flags & 0x0001 != 0 {
                                report.dirty = true;
                                report.reasons.push("$Volume dirty flag set".into());
                            }
                        }
                    }
                }
            }
        }
    }
    // Look for hiberfil.sys / EFS reparse in the root (MFT #5).
    if let Ok(names) = vol.list_root_names(dev) {
        for n in names {
            let lower = n.to_ascii_lowercase();
            if lower == "hiberfil.sys" {
                report.hibernated = true;
                report.reasons.push("hiberfil.sys present (Windows fast startup)".into());
            }
        }
    }
    // Scan a handful of MFT records for the ENCRYPTED attribute flag (EFS).
    for rec_n in 0..64u64 {
        if let Ok(rec) = vol.read_record(dev, rec_n) {
            if look_for_efs(&rec) {
                report.efs_present = true;
                report.reasons.push(format!("EFS-encrypted attribute in MFT #{rec_n}"));
                break;
            }
        }
    }
    Ok(())
}

fn look_for_efs(rec: &MftRecord) -> bool {
    rec.attributes().ok().map(|attrs| {
        attrs.iter().any(|a| a.header().flags.contains(crate::attribute::AttributeFlags::ENCRYPTED))
    }).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_never_writes() {
        let mut r = SafetyReport {
            verified: true,
            ..Default::default()
        };
        assert!(!r.writable(WritePolicy::ReadOnly));
        r.dirty = true;
        assert!(!r.writable(WritePolicy::ReadWriteIfSafe));
        assert!(r.writable(WritePolicy::ForceWrite));
        r.bitlocker = true;
        assert!(!r.writable(WritePolicy::ForceWrite));
    }

    #[test]
    fn unverified_probe_denies_write() {
        let r = SafetyReport::default();
        assert!(!r.verified);
        assert!(!r.writable(WritePolicy::ReadWriteIfSafe));
        assert!(!r.writable(WritePolicy::ForceWrite));
    }
}
