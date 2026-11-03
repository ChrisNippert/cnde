//! Mounted drives, from /proc/self/mountinfo.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug)]
pub struct Drive {
    pub name: String,
    pub mount: PathBuf,
    /// e.g. "/dev/nvme0n1p2".
    pub device: String,
    pub fs_type: String,
    pub total: u64,
    /// Bytes available to the user (not root-reserved).
    pub free: u64,
    pub removable: bool,
}

/// Mounted real drives: System first, then fixed, then removable.
pub fn drives() -> Vec<Drive> {
    let Ok(text) = fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let labels = labels();
    let mut out: Vec<Drive> = parse_mounts(&text)
        .into_iter()
        .filter_map(|m| {
            let st = rustix::fs::statvfs(&m.mount).ok()?;
            let unit = st.f_frsize;
            let name = if m.mount == Path::new("/") {
                "System".to_string()
            } else {
                labels.get(&m.device).cloned().unwrap_or_else(|| {
                    m.mount
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| m.device.rsplit('/').next().unwrap_or("").to_string())
                })
            };
            let removable = m.mount.starts_with("/run/media")
                || m.mount.starts_with("/media")
                || is_removable(&m.device);
            Some(Drive {
                name,
                total: st.f_blocks * unit,
                free: st.f_bavail * unit,
                removable,
                mount: m.mount,
                device: m.device,
                fs_type: m.fs_type,
            })
        })
        .collect();
    out.sort_by_cached_key(|d| {
        (
            d.mount != Path::new("/"),
            d.removable,
            d.name.to_lowercase(),
        )
    });
    out
}

/// Unmount a drive and, if it's a whole removable disk, power it off.
pub fn eject(drive: &Drive) -> io::Result<()> {
    if !drive.device.starts_with("/dev/") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "This isn't a plain disk, so it can't be ejected here",
        ));
    }
    if !on_path("udisksctl") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Ejecting needs udisks (the udisksctl command), which isn't installed",
        ));
    }
    let out = Command::new("udisksctl")
        .args(["unmount", "-b", &drive.device])
        .output()?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(io::Error::other(if msg.is_empty() {
            "Couldn't unmount the drive".to_string()
        } else {
            msg
        }));
    }
    let _ = Command::new("udisksctl")
        .args(["power-off", "-b", &parent_disk(&drive.device)])
        .output();
    Ok(())
}

fn on_path(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

struct Mount {
    mount: PathBuf,
    device: String,
    fs_type: String,
}

/// Mounts worth showing: one per block device (btrfs subvolumes and bind
/// mounts collapse to the shortest path), plus useful ZFS datasets.
fn parse_mounts(text: &str) -> Vec<Mount> {
    let mut best: HashMap<String, Mount> = HashMap::new();
    let mut order = Vec::new();
    let mut zfs = Vec::new();
    for line in text.lines() {
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let mount = unescape(pre.split(' ').nth(4).unwrap_or(""));
        let mut p = post.split(' ');
        let (fs_type, source) = (p.next().unwrap_or(""), unescape(p.next().unwrap_or("")));
        if skip_mount(&mount) {
            continue;
        }
        if fs_type == "zfs" {
            zfs.push((source, mount));
            continue;
        }
        if !source.starts_with("/dev/") || source.starts_with("/dev/loop") {
            continue;
        }
        let m = Mount {
            mount: PathBuf::from(&mount),
            device: source.clone(),
            fs_type: fs_type.to_string(),
        };
        match best.get_mut(&source) {
            // Subvolumes and bind mounts: the shortest path wins, so
            // a btrfs filesystem is one drive however many subvolumes it has.
            Some(old) => {
                if m.mount.as_os_str().len() < old.mount.as_os_str().len() {
                    *old = m;
                }
            }
            None => {
                order.push(source.clone());
                best.insert(source, m);
            }
        }
    }
    let mut out: Vec<Mount> = order.into_iter().filter_map(|k| best.remove(&k)).collect();
    out.extend(zfs_mounts(&zfs));
    out
}

/// Which ZFS datasets (dataset, mountpoint) get their own drive. The one at
/// `/` is the system; others show unless they sit on a system path or are
/// nested inside another shown dataset of the same pool (home/user in home).
fn zfs_mounts(datasets: &[(String, String)]) -> Vec<Mount> {
    let pool = |d: &str| d.split('/').next().unwrap_or(d).to_string();
    let under = |m: &str, p: &str| m.strip_prefix(p).is_some_and(|r| r.starts_with('/'));
    let system = |m: &str| {
        [
            "/usr", "/etc", "/opt", "/root", "/tmp", "/sys", "/proc", "/dev",
        ]
        .iter()
        .any(|p| m == *p || under(m, p))
    };
    let mut out: Vec<Mount> = Vec::new();
    let mut shown: Vec<(&str, &str)> = Vec::new();
    // Shortest mountpoints first so parents are seen before children.
    let mut sorted: Vec<&(String, String)> = datasets.iter().collect();
    sorted.sort_by_key(|(_, m)| m.len());
    for (ds, mount) in sorted {
        let root = mount == "/";
        if !root {
            if system(mount) {
                continue;
            }
            let nested = shown
                .iter()
                .any(|(d, m)| pool(d) == pool(ds) && *m != "/" && under(mount, m));
            if nested {
                continue;
            }
        } else if out.iter().any(|m| m.mount == Path::new("/")) {
            continue;
        }
        shown.push((ds, mount));
        out.push(Mount {
            mount: PathBuf::from(mount),
            device: ds.clone(),
            fs_type: format!("zfs ({})", pool(ds)),
        });
    }
    out
}

fn skip_mount(m: &str) -> bool {
    let under = |p: &str| m == p || m.strip_prefix(p).is_some_and(|r| r.starts_with('/'));
    if m == "/efi" || under("/boot") || under("/var") || under("/snap") {
        return true;
    }
    under("/run") && !under("/run/media")
}

/// Mountinfo escapes space, tab, newline and backslash as octal.
fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && let Some(v) = s
                .get(i + 1..i + 4)
                .and_then(|o| u8::from_str_radix(o, 8).ok())
        {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Filesystem labels keyed by the device path they point at.
fn labels() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Ok(rd) = fs::read_dir("/dev/disk/by-label") else {
        return map;
    };
    for de in rd.flatten() {
        let Ok(target) = fs::canonicalize(de.path()) else {
            continue;
        };
        let name = decode_label(&de.file_name().to_string_lossy());
        map.insert(target.to_string_lossy().into_owned(), name);
    }
    map
}

/// by-label names escape odd bytes as \xNN.
fn decode_label(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && let Some(v) = s
                .get(i + 2..i + 4)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "/dev/sdb1" -> "/dev/sdb"; "/dev/nvme0n1p2" -> "/dev/nvme0n1".
fn parent_disk(device: &str) -> String {
    let Some(name) = device.strip_prefix("/dev/") else {
        return device.to_string();
    };
    if let Ok(real) = fs::canonicalize(format!("/sys/class/block/{name}"))
        && real.join("partition").exists()
        && let Some(parent) = real.parent().and_then(|p| p.file_name())
    {
        return format!("/dev/{}", parent.to_string_lossy());
    }
    device.to_string()
}

fn is_removable(device: &str) -> bool {
    let disk = parent_disk(device);
    let name = disk.strip_prefix("/dev/").unwrap_or(&disk);
    // Check the partition itself first, then its disk.
    [device, disk.as_str()].iter().any(|d| {
        let n = d.strip_prefix("/dev/").unwrap_or(d);
        fs::read_to_string(format!("/sys/class/block/{n}/removable")).is_ok_and(|s| s.trim() == "1")
    }) && !name.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
24 1 259:2 /@ / rw,relatime shared:1 - btrfs /dev/nvme0n1p2 rw,subvolid=256
25 24 259:2 /@home /home rw,relatime shared:2 - btrfs /dev/nvme0n1p2 rw,subvolid=257
26 24 259:1 / /boot rw shared:3 - vfat /dev/nvme0n1p1 rw
27 24 0:5 / /dev rw shared:4 - devtmpfs devtmpfs rw
28 24 7:0 / /mnt/img rw - ext4 /dev/loop0 rw
29 24 8:17 / /run/media/u/My\\040Stick rw - exfat /dev/sdb1 rw
30 24 8:33 / /run/user/1000/doc rw - fuse /dev/sdc1 rw
31 24 259:3 / /var/lib/x rw - ext4 /dev/nvme0n1p3 rw
32 24 8:49 / /mnt/data rw - ext4 /dev/sdd1 rw
";

    #[test]
    fn filters_and_dedups() {
        let got = parse_mounts(MOUNTINFO);
        let got: Vec<_> = got
            .iter()
            .map(|m| (m.mount.to_str().unwrap(), m.device.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/", "/dev/nvme0n1p2"),
                ("/run/media/u/My Stick", "/dev/sdb1"),
                ("/mnt/data", "/dev/sdd1"),
            ]
        );
    }

    const BTRFS: &str = "\
24 1 0:30 /@ / rw,relatime shared:1 - btrfs /dev/mapper/root rw,subvolid=256,subvol=/@
25 24 0:30 /@home /home rw,relatime shared:2 - btrfs /dev/mapper/root rw,subvolid=257,subvol=/@home
26 24 0:30 /@snapshots /.snapshots rw shared:3 - btrfs /dev/mapper/root rw,subvol=/@snapshots
27 24 0:30 /@log /var/log rw shared:4 - btrfs /dev/mapper/root rw,subvol=/@log
28 24 8:17 / /run/media/user/USB rw - btrfs /dev/sdb1 rw,subvolid=5,subvol=/
";

    const ZFS: &str = "\
24 1 0:40 / / rw,relatime shared:1 - zfs rpool/ROOT/arch rw,xattr
25 24 0:41 / /home rw shared:2 - zfs rpool/home rw,xattr
26 24 0:42 / /home/user rw shared:3 - zfs rpool/home/user rw,xattr
27 24 0:43 / /var/log rw shared:4 - zfs rpool/var/log rw,xattr
28 24 0:44 / /usr/share rw shared:5 - zfs rpool/usr rw,xattr
29 24 0:45 / /mnt/media rw shared:6 - zfs tank/media rw,xattr
30 24 0:46 / /mnt/media/old rw shared:7 - zfs tank/media/old rw,xattr
31 24 0:47 / /home/user/vm rw shared:8 - zfs tank/vm rw,xattr
";

    #[test]
    fn btrfs_is_one_drive_per_filesystem() {
        let got = parse_mounts(BTRFS);
        let got: Vec<_> = got
            .iter()
            .map(|m| (m.mount.to_str().unwrap(), m.device.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("/", "/dev/mapper/root"),
                ("/run/media/user/USB", "/dev/sdb1"),
            ]
        );
    }

    #[test]
    fn zfs_shows_useful_datasets() {
        let got = parse_mounts(ZFS);
        let got: Vec<_> = got
            .iter()
            .map(|m| {
                (
                    m.mount.to_str().unwrap(),
                    m.device.as_str(),
                    m.fs_type.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("/", "rpool/ROOT/arch", "zfs (rpool)"),
                ("/home", "rpool/home", "zfs (rpool)"),
                ("/mnt/media", "tank/media", "zfs (tank)"),
                ("/home/user/vm", "tank/vm", "zfs (tank)"),
            ]
        );
    }

    #[test]
    fn skips_and_escapes() {
        assert!(skip_mount("/boot/efi") && skip_mount("/efi") && skip_mount("/var/lib"));
        assert!(skip_mount("/run/user/1000") && !skip_mount("/run/media/u/x"));
        assert!(!skip_mount("/bootleg") && !skip_mount("/home"));
        assert_eq!(unescape("a\\040b"), "a b");
        assert_eq!(decode_label("My\\x20Disk"), "My Disk");
    }

    #[test]
    fn real_drives_do_not_panic() {
        // Whatever this machine has, the root filesystem should lead if shown.
        let d = drives();
        if let Some(i) = d.iter().position(|d| d.name == "System") {
            assert_eq!(i, 0);
        }
        for x in &d {
            assert!(x.free <= x.total);
        }
    }
}
