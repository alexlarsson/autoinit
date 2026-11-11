//! Root lookup without udev or libblkid. ext4 identifiers and GPT/MBR partitions.
use anyhow::{Context, Result, bail, ensure};
use std::fs::{self, File};
use std::os::unix::fs::{FileExt, FileTypeExt};
use std::path::{Path, PathBuf};
use uuid::Uuid;

fn read_at(path: &Path, offset: u64, length: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0; length];
    File::open(path)
        .with_context(|| format!("open {path:?}"))?
        .read_exact_at(&mut bytes, offset)
        .with_context(|| format!("read {length} bytes at offset {offset} from {path:?}"))?;
    Ok(bytes)
}

/// Read a decimal sysfs attribute while retaining its path in error reports.
fn read_number(path: &Path) -> Result<u64> {
    fs::read_to_string(path)
        .with_context(|| format!("read {path:?}"))?
        .trim()
        .parse()
        .with_context(|| format!("parse number in {path:?}"))
}

/// Read a partition's GPT UUID/name or MBR identifier using its sysfs entry.
/// Check GPT offsets against the kernel's view before accepting the entry.
fn partition(sys: &Path) -> Result<(String, String)> {
    let index = read_number(&sys.join("partition"))?;
    ensure!(index > 0, "partition number is zero");

    let real = sys
        .canonicalize()
        .with_context(|| format!("resolve sysfs path {sys:?}"))?;
    let parent = real.parent().context("parent block device")?;
    let device = Path::new("/dev").join(parent.file_name().context("disk name")?);

    // sysfs reports start in 512-byte units even on 4Kn devices.
    let start = read_number(&sys.join("start"))?;
    let sector = read_number(&parent.join("queue/logical_block_size"))?;
    ensure!(
        matches!(sector, 512 | 4096),
        "unsupported logical sector size"
    );

    let head = read_at(&device, sector, 92)?;
    if &head[..8] == b"EFI PART" {
        let entries = u64::from_le_bytes(head[72..80].try_into()?);
        let count = u32::from_le_bytes(head[80..84].try_into()?) as u64;
        let size = u32::from_le_bytes(head[84..88].try_into()?) as u64;
        ensure!(
            index <= count && (128..=4096).contains(&size),
            "invalid GPT entry bounds"
        );

        let offset = entries
            .checked_mul(sector)
            .and_then(|n| n.checked_add((index - 1).checked_mul(size)?))
            .context("GPT entry offset overflow")?;
        let entry = read_at(&device, offset, 128)?;
        ensure!(
            u64::from_le_bytes(entry[32..40].try_into()?).checked_mul(sector)
                == start.checked_mul(512),
            "GPT start differs from kernel partition"
        );

        let utf16: Vec<u16> = entry[56..128]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .take_while(|c| *c != 0)
            .collect();
        return Ok((
            Uuid::from_slice_le(&entry[16..32])?.to_string(),
            String::from_utf16(&utf16).context("decode GPT partition label")?,
        ));
    }

    let mbr = read_at(&device, 0, 512)?;
    ensure!(&mbr[510..512] == b"\x55\xaa", "no GPT or MBR");

    Ok((
        format!(
            "{:08x}-{index:02x}",
            u32::from_le_bytes(mbr[440..444].try_into()?)
        ),
        String::new(),
    ))
}

/// Match an identifier against ext4 superblock or GPT/MBR partition metadata,
/// without relying on udev-created device symlinks.
fn matches(sys: &Path, key: &str, wanted: &str) -> Result<bool> {
    // Unattached loop devices and empty removable drives are normal, and
    // should not crowd real device failures out of the probe diagnostics.
    if read_number(&sys.join("size"))? == 0 {
        return Ok(false);
    }

    match key {
        "UUID" | "LABEL" => {
            let sb = read_at(
                &Path::new("/dev").join(sys.file_name().context("device name")?),
                1024,
                136,
            )?;
            if &sb[56..58] != b"\x53\xef" {
                return Ok(false);
            }

            if key == "UUID" {
                Ok(Uuid::from_slice(&sb[104..120])?
                    .to_string()
                    .eq_ignore_ascii_case(wanted))
            } else {
                Ok(sb[120..136].split(|b| *b == 0).next().unwrap_or_default() == wanted.as_bytes())
            }
        }
        "PARTUUID" | "PARTLABEL" => {
            // Whole disks cannot match partition identifiers.
            if !sys
                .join("partition")
                .try_exists()
                .with_context(|| format!("check partition attribute in {sys:?}"))?
            {
                return Ok(false);
            }

            let (id, label) = partition(sys)?;
            Ok(if key == "PARTUUID" {
                id.eq_ignore_ascii_case(wanted)
            } else {
                label == wanted
            })
        }
        _ => bail!("unsupported root identifier {key}"),
    }
}

/// Resolve a device path or root identifier to exactly one block device.
/// Retain failed probes for diagnostics and reject ambiguous identifiers.
pub fn resolve(spec: &str) -> Result<PathBuf> {
    if spec.starts_with("/dev/") {
        let p = PathBuf::from(spec);
        ensure!(
            p.metadata()
                .with_context(|| format!("stat root device {p:?}"))?
                .file_type()
                .is_block_device(),
            "root {p:?} is not a block device"
        );
        return Ok(p);
    }

    let (key, wanted) = spec
        .split_once('=')
        .context("root must be /dev/... or UUID/LABEL/PARTUUID/PARTLABEL=...")?;
    ensure!(
        ["UUID", "LABEL", "PARTUUID", "PARTLABEL"].contains(&key),
        "unsupported root identifier {key}"
    );

    let mut found = Vec::new();
    let mut errors = Vec::new();
    for sys in fs::read_dir("/sys/class/block").context("enumerate /sys/class/block")? {
        let sys = sys.context("read /sys/class/block entry")?.path();
        match matches(&sys, key, wanted) {
            Ok(true) => found.push(Path::new("/dev").join(sys.file_name().unwrap())),
            Ok(false) => {}
            Err(error) if errors.len() < 3 => errors.push(format!("{sys:?}: {error:#}")),
            Err(_) => {}
        }
    }

    ensure!(
        found.len() == 1,
        "root {spec} matched {} devices ({found:?}); probe errors: {}",
        found.len(),
        if errors.is_empty() {
            "none".to_owned()
        } else {
            errors.join("; ")
        }
    );
    Ok(found.remove(0))
}
