//! Optional early networking, partition growth and online ext4 resizing.

use anyhow::{Context, Result, ensure};
use rustix::fs::{StatVfsMountFlags, ext4_ioc_resize_fs, fstatfs, major, minor};
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

use crate::{debug, log};

/// Apply kernel-only network state after drivers load, without starting a daemon.
pub fn setup_network() {
    let config = "/etc/autoinit.nmstate";
    if !Path::new(config).exists() {
        debug(format_args!("no early network configuration"));
        return;
    }

    debug(format_args!(
        "applying kernel-only network configuration from {config}"
    ));

    #[cfg(feature = "nmstate")]
    let result = apply_network(config);

    #[cfg(not(feature = "nmstate"))]
    let result: Result<()> = Err(anyhow::anyhow!("init binary lacks nmstate support"));

    match result {
        Ok(()) => log("early network configured"),
        Err(error) => log(format!("early network configuration failed: {error:#}")),
    }
}

/// Apply the same kernel-only, unverified state as nmstatectl, without its CLI.
#[cfg(feature = "nmstate")]
fn apply_network(config: &str) -> Result<()> {
    let yaml = fs::read_to_string(config)
        .with_context(|| format!("read nmstate configuration {config}"))?;
    let mut state = nmstate::NetworkState::new_from_yaml(&yaml)?;
    state.set_kernel_only(true).set_verify_change(false);
    state.apply()?;
    Ok(())
}

/// Read the mounted physical root device's current capacity. sysfs uses 512-byte
/// sectors independently of the disk's logical or physical sector size.
fn root_device_size() -> Result<u64> {
    let device = fs::metadata("/sysroot")
        .context("stat physical /sysroot")?
        .dev();
    let path = format!("/sys/dev/block/{}:{}/size", major(device), minor(device));
    let sectors: u64 = fs::read_to_string(&path)
        .with_context(|| format!("read root device capacity {path}"))?
        .trim()
        .parse()
        .with_context(|| format!("parse root device capacity {path}"))?;

    sectors
        .checked_mul(512)
        .context("root device size overflow")
}

/// Repart has the same successful exit status for changes and no changes. Compare
/// the kernel's root partition sizes instead, including after a partial failure.
fn repart_root() -> Result<bool> {
    let before = root_device_size()?;
    debug(format_args!(
        "running systemd-repart on /sysroot with /usr/lib/repart.d; root device size {before} bytes"
    ));
    let result = Command::new("/usr/bin/systemd-repart")
        .args([
            "--dry-run=no",
            "--root=/sysroot",
            "--definitions=/usr/lib/repart.d",
        ])
        .status();

    match result {
        Ok(status) if status.success() => {}
        Ok(status) => log(format!("systemd-repart failed: {status}")),
        Err(error) => log(format!("could not run systemd-repart: {error}")),
    }

    let after = root_device_size()?;
    debug(format_args!(
        "root device size after repart: {after} bytes (before: {before})"
    ));
    Ok(after > before)
}

/// Resize the mounted ext4 filesystem to its current device capacity, including
/// space made available before boot by an image-writing tool.
fn grow_root() -> Result<()> {
    let root = File::open("/sysroot").context("open /sysroot for filesystem growth")?;
    let stat = fstatfs(&root).context("stat filesystem on /sysroot")?;
    ensure!(stat.f_type == 0xef53, "root filesystem is not ext4");
    ensure!(
        stat.f_flags as u64 & StatVfsMountFlags::RDONLY.bits() == 0,
        "root filesystem is read-only"
    );
    ensure!(stat.f_bsize > 0, "invalid filesystem block size");

    let bytes = root_device_size()?;
    let blocks = bytes / stat.f_bsize as u64;
    ensure!(blocks > 0, "root device is too small");

    debug(format_args!(
        "resizing ext4 on /sysroot to {blocks} blocks of {} bytes",
        stat.f_bsize
    ));
    ext4_ioc_resize_fs(&root, blocks).context("resize ext4")?;
    log(format!("ext4 grown to device capacity ({bytes} bytes)"));
    Ok(())
}

/// Keep optional storage work before composefs hides the physical root mount.
/// Like the previous services, failures are reported without preventing boot.
pub fn prepare_root() {
    let mut grow = Path::new("/etc/autoinit.growfs").exists();
    let repart = Path::new("/etc/autoinit.repart").exists();
    debug(format_args!(
        "optional storage setup: repart={repart}, unconditional growfs={grow}"
    ));

    if repart {
        match repart_root() {
            Ok(changed) => grow |= changed,
            Err(error) => log(format!("repartitioning failed: {error:#}")),
        }
    }

    if grow && let Err(error) = grow_root() {
        log(format!("filesystem growth failed: {error:#}"));
    }
}
