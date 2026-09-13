//! Disk discovery / unmount helpers.
//!
//! On macOS this talks to `diskutil` so we can take an NTFS volume away from
//! Paragon or Apple's read-only probe and open `/dev/rdisk*`.

use ntfs_core::{Error, Result};

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::process::Command;

    pub fn list_ntfs_bsd_names() -> Result<Vec<String>> {
        let mut out = Vec::new();
        if let Ok(list) = diskutil(&["list"]) {
            let list_text = String::from_utf8_lossy(&list);
            for line in list_text.lines() {
                let lower = line.to_ascii_lowercase();
                if lower.contains("ntfs") || lower.contains("windows_ntfs") {
                    if let Some(id) = line.split_whitespace().last() {
                        if id.starts_with("disk") {
                            out.push(id.to_string());
                        }
                    }
                }
            }
        }
        for bsd in ntfs_from_mount_table() {
            out.push(bsd);
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    fn ntfs_from_mount_table() -> Vec<String> {
        let Ok(out) = Command::new("/sbin/mount").output() else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let lower = line.to_ascii_lowercase();
            if !(lower.contains("ntfs") || lower.contains("windows_ntfs")) {
                continue;
            }
            let Some(dev) = line.split_whitespace().next() else {
                continue;
            };
            let name = dev.trim_start_matches("/dev/");
            let bsd = name
                .strip_prefix('r')
                .filter(|rest| rest.starts_with("disk"))
                .unwrap_or(name);
            if bsd.starts_with("disk") {
                found.push(bsd.to_string());
            }
        }
        found
    }

    pub fn friendly_volume_name(
        volume_name: Option<&str>,
        mount_point: Option<&str>,
        bsd: &str,
    ) -> String {
        if let Some(n) = volume_name.map(str::trim) {
            if !n.is_empty() && !n.eq_ignore_ascii_case("not applicable") {
                return n.to_string();
            }
        }
        if let Some(mp) = mount_point {
            if let Some(base) = mp.rsplit('/').next() {
                if !base.is_empty() && base != "Volumes" {
                    return base.to_string();
                }
            }
        }
        format!("NTFS USB ({bsd})")
    }

    pub fn unmount_and_claim(bsd: &str) -> Result<()> {
        let slice = bsd.trim_start_matches("/dev/");
        if !valid_slice(slice) {
            return Err(Error::Safety(format!("invalid disk slice {bsd}")));
        }
        if is_system_volume(slice) {
            return Err(Error::Safety(format!(
                "refusing to unmount internal or boot disk {slice}"
            )));
        }
        force_unmount_slice(slice);
        if is_mounted(slice) {
            std::thread::sleep(std::time::Duration::from_millis(150));
            force_unmount_slice(slice);
        }
        if is_mounted(slice) {
            return Err(Error::Safety(format!(
                "could not unmount {slice}; still mounted (FSKit or another driver)"
            )));
        }
        Ok(())
    }

    fn valid_slice(name: &str) -> bool {
        let rest = match name.strip_prefix("disk") {
            Some(r) => r,
            None => return false,
        };
        let mut chars = rest.chars().peekable();
        if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            chars.next();
        }
        if chars.next() != Some('s') {
            return false;
        }
        if !chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            return false;
        }
        while chars.peek().is_some_and(|c| c.is_ascii_digit()) {
            chars.next();
        }
        chars.next().is_none()
    }

    fn force_unmount_slice(slice: &str) {
        let _ = Command::new("/usr/sbin/diskutil")
            .args(["unmount", "force", slice])
            .output();
    }

    fn is_system_volume(slice: &str) -> bool {
        let Ok(out) = Command::new("/usr/sbin/diskutil")
            .args(["info", slice])
            .output()
        else {
            return true;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let internal = field(&text, "Internal:").is_some_and(|v| v.eq_ignore_ascii_case("yes"));
        let loc = field(&text, "Device Location:").is_some_and(|v| v.eq_ignore_ascii_case("internal"));
        let boot = field(&text, "Boot Volume:").is_some_and(|v| v.eq_ignore_ascii_case("yes"));
        let mp = field(&text, "Mount Point:").unwrap_or_default();
        let system_mp = mp == "/" || mp.starts_with("/System/Volumes/");
        internal || loc || boot || system_mp
    }

    pub fn rdisk_path(bsd: &str) -> String {
        let name = bsd.trim_start_matches("/dev/");
        format!("/dev/r{name}")
    }

    /// Best-effort summary from `diskutil info` for probe output.
    pub fn disk_summary(bsd: &str) -> Result<super::DiskSummary> {
        let out = diskutil(&["info", bsd])?;
        let text = String::from_utf8_lossy(&out);
        Ok(super::DiskSummary {
            volume_name: Some(friendly_volume_name(
                field(&text, "Volume Name:").as_deref(),
                field(&text, "Mount Point:").as_deref(),
                bsd.trim_start_matches("/dev/"),
            )),
            mount_point: field(&text, "Mount Point:"),
            mounted: field(&text, "Mounted:").map(|s| s.eq_ignore_ascii_case("yes")),
            file_system: field(&text, "File System Personality:"),
            device_node: field(&text, "Device Node:"),
            bsd: bsd.trim_start_matches("/dev/").to_string(),
        })
    }

    /// True when the current user can open the raw device read-only.
    pub fn can_open_rdisk_ro(bsd: &str) -> bool {
        let path = rdisk_path(bsd);
        std::fs::File::open(&path).is_ok()
    }

    #[cfg(target_os = "macos")]
    pub fn macos_raw_access_hint() -> &'static str {
        if in_operator_group() {
            "raw device readable (operator group member)"
        } else {
            "raw device needs operator group or sudo (admin alone is insufficient)"
        }
    }

    #[cfg(not(target_os = "macos"))]
    pub fn macos_raw_access_hint() -> &'static str {
        "see device permissions"
    }

    fn field(text: &str, key: &str) -> Option<String> {
        text.lines()
            .map(str::trim)
            .find(|l| l.starts_with(key))
            .map(|l| l[key.len()..].trim().to_string())
            .filter(|s| !s.is_empty())
    }

    #[cfg(target_os = "macos")]
    fn in_operator_group() -> bool {
        use std::process::Command;
        let Ok(out) = Command::new("id").arg("-Gn").output() else {
            return false;
        };
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .any(|g| g == "operator")
    }

    pub fn whole_disk(bsd: &str) -> String {
        let name = bsd.trim_start_matches("/dev/");
        // disk4s1 → disk4; disk0s2 → disk0
        if let Some(i) = name.rfind('s') {
            if i > 0 && name.as_bytes()[i - 1].is_ascii_digit() {
                return name[..i].to_string();
            }
        }
        name.to_string()
    }

    fn is_mounted(bsd: &str) -> bool {
        let name = bsd.trim_start_matches("/dev/");
        let info_mounted = diskutil_best_effort(&["info", name])
            .and_then(|s| field(&s, "Mounted:"))
            .map(|v| v.eq_ignore_ascii_case("yes"))
            .unwrap_or(false);
        if info_mounted {
            return true;
        }
        mount_table_has(name)
    }

    fn mount_table_has(bsd: &str) -> bool {
        let Ok(out) = Command::new("/sbin/mount").output() else {
            return false;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let needle = format!("/dev/{bsd} on ");
        text.lines().any(|l| l.contains(&needle))
    }

    fn diskutil_best_effort(args: &[&str]) -> Option<String> {
        Command::new("/usr/sbin/diskutil")
            .args(args)
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn diskutil(args: &[&str]) -> Result<Vec<u8>> {
        let out = Command::new("/usr/sbin/diskutil")
            .args(args)
            .output()
            .map_err(Error::from)?;
        if !out.status.success() {
            return Err(Error::Io(std::io::Error::other(format!(
                "diskutil {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ))));
        }
        Ok(out.stdout)
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::*;
    pub fn list_ntfs_bsd_names() -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    pub fn unmount_and_claim(_bsd: &str) -> Result<()> {
        Ok(())
    }
    pub fn rdisk_path(bsd: &str) -> String {
        format!("/dev/{bsd}")
    }
    pub fn disk_summary(bsd: &str) -> Result<super::DiskSummary> {
        Ok(super::DiskSummary {
            bsd: bsd.to_string(),
            volume_name: None,
            mount_point: None,
            mounted: None,
            file_system: None,
            device_node: None,
        })
    }
    pub fn can_open_rdisk_ro(_bsd: &str) -> bool {
        false
    }
    pub fn macos_raw_access_hint() -> &'static str {
        "n/a"
    }
    pub fn whole_disk(bsd: &str) -> String {
        bsd.to_string()
    }
    pub fn friendly_volume_name(
        volume_name: Option<&str>,
        _mount_point: Option<&str>,
        bsd: &str,
    ) -> String {
        volume_name
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or(bsd)
            .to_string()
    }
}

pub use imp::*;

#[derive(Debug, Clone)]
pub struct DiskSummary {
    pub bsd: String,
    pub volume_name: Option<String>,
    pub mount_point: Option<String>,
    pub mounted: Option<bool>,
    pub file_system: Option<String>,
    pub device_node: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdisk_naming() {
        #[cfg(target_os = "macos")]
        {
            assert_eq!(rdisk_path("disk4s1"), "/dev/rdisk4s1");
            assert_eq!(whole_disk("disk4s1"), "disk4");
            assert_eq!(whole_disk("/dev/disk0s2"), "disk0");
            assert!(unmount_and_claim("sda1").is_err());
            assert!(unmount_and_claim("disk").is_err());
            assert_eq!(
                friendly_volume_name(None, Some("/Volumes/Untitled"), "disk4s1"),
                "Untitled"
            );
            assert_eq!(
                friendly_volume_name(Some(""), Some("/Volumes/Untitled"), "disk4s1"),
                "Untitled"
            );
            assert_eq!(
                friendly_volume_name(Some("Backup"), Some("/Volumes/Untitled"), "disk4s1"),
                "Backup"
            );
        }
    }

    #[test]
    fn refuse_internal_and_nonslices() {
        #[cfg(target_os = "macos")]
        {
            let whole = unmount_and_claim("disk0").unwrap_err().to_string();
            assert!(
                whole.contains("internal")
                    || whole.contains("boot")
                    || whole.contains("invalid")
                    || whole.contains("refusing"),
                "disk0: {whole}"
            );
            let boot = unmount_and_claim("disk0s1").unwrap_err().to_string();
            assert!(
                boot.contains("internal") || boot.contains("boot") || boot.contains("refusing"),
                "disk0s1: {boot}"
            );
            let whole = unmount_and_claim("disk4").unwrap_err().to_string();
            assert!(
                whole.contains("invalid") || whole.contains("refusing"),
                "disk4: {whole}"
            );
        }
    }
}
