//! Native composefs root setup, adapted from bootc crates/initramfs

use std::{
    fs,
    io::ErrorKind,
    os::fd::{AsFd, OwnedFd},
    path::Path,
};

use anyhow::{Context, Result, bail, ensure};
use composefs::{
    fsverity::{Algorithm, FsVerityHashValue, Sha512HashValue},
    mount::{FsHandle, MountOptions, mount_at},
    mountcompat::{overlayfs_set_fd, overlayfs_set_lower_and_data_fds, prepare_mount},
    repository::{ImageNotFound, IntegrityPolicy, Repository, VerityState},
};
use linux_kernel_cmdline::utf8::Cmdline;
use rustix::{
    fs::{CWD, Mode, OFlags, StatVfsMountFlags, mkdirat, openat, statvfs},
    io::Errno,
    mount::{
        FsMountFlags, MountAttrFlags, MountFlags, OpenTreeFlags, fsconfig_create,
        fsconfig_set_string, fsmount, mount_remount, open_tree,
    },
};
use serde::Deserialize;

use crate::debug;

pub(crate) struct ComposefsImage {
    digest: Sha512HashValue,
    insecure: bool,
}

/// Parse values directly from the shared kernel command-line parser. Format
/// version does not affect mounting: composefs reads it from the EROFS image.
/// Keep candidate order and require a consistent fs-verity policy for fallback.
pub(crate) fn parse_composefs_images(cmdline: &Cmdline<'_>) -> Result<Vec<ComposefsImage>> {
    let mut images: Vec<ComposefsImage> = Vec::new();
    let mut seen_legacy = false;

    for parameter in cmdline.iter() {
        let key = parameter.key();
        if &*key != "composefs" && &*key != "composefs.digest" {
            continue;
        }

        let value = parameter
            .value()
            .with_context(|| format!("{key}= requires a value"))?;
        let insecure = value.starts_with('?');
        let value = value.strip_prefix('?').unwrap_or(value);

        let hex = if &*key == "composefs" {
            ensure!(!seen_legacy, "duplicate composefs= kernel argument");
            seen_legacy = true;
            value
        } else {
            let (descriptor, hex) = value
                .split_once(':')
                .context("expected composefs.digest=<version>-<hash>-<blocksize>:<hex>")?;
            let (version, algorithm) = descriptor
                .split_once('-')
                .context("missing composefs format version")?;
            ensure!(
                matches!(version, "v1" | "v2"),
                "unsupported composefs version {version}"
            );

            let algorithm: Algorithm = format!("fsverity-{algorithm}").parse()?;
            // The repository and deployment layout use SHA-512. Other valid
            // algorithms may appear alongside it in a multi-format UKI.
            if algorithm != Algorithm::SHA512 {
                continue;
            }
            hex
        };

        let digest =
            Sha512HashValue::from_hex(hex).with_context(|| format!("invalid {key}= digest"))?;
        if let Some(first) = images.first() {
            ensure!(
                first.insecure == insecure,
                "composefs candidates have mixed fs-verity policy"
            );
        }
        debug(format_args!(
            "composefs candidate {}: {hex}, insecure={insecure}",
            images.len() + 1
        ));
        images.push(ComposefsImage { digest, insecure });
    }

    ensure!(
        !images.is_empty(),
        "no SHA-512 composefs image in kernel command line"
    );
    Ok(images)
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MountType {
    #[serde(alias = "root")]
    None,
    Bind,
    Overlay,
    #[serde(alias = "volatile")]
    Transient,
}

#[derive(Default, Deserialize)]
struct MountConfig {
    mount: Option<MountType>,
    #[serde(default)]
    transient: bool,
}

#[derive(Default, Deserialize)]
struct RootConfig {
    #[serde(default)]
    transient: bool,
}

#[derive(Default, Deserialize)]
struct Config {
    #[serde(default)]
    etc: MountConfig,
    #[serde(default)]
    var: MountConfig,
    #[serde(default)]
    root: RootConfig,
}

/// Read the image's mount policy, defaulting to persistent /etc and /var binds.
/// Honor systemd's volatile-state request unless /var is explicitly configured.
fn read_config(cmdline: &Cmdline<'_>) -> Result<Config> {
    let mut config: Config = match fs::read_to_string("/usr/lib/composefs/setup-root-conf.toml") {
        Ok(text) => {
            debug(format_args!(
                "reading mount policy from /usr/lib/composefs/setup-root-conf.toml"
            ));
            toml::from_str(&text).context("parse /usr/lib/composefs/setup-root-conf.toml")?
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            debug(format_args!(
                "no setup-root configuration; using persistent /etc and /var defaults"
            ));
            Config::default()
        }
        Err(error) => return Err(error).context("read /usr/lib/composefs/setup-root-conf.toml"),
    };

    // Leave /var for systemd-fstab-generator when it requests volatile state,
    // unless the image explicitly selected a different /var configuration.
    if matches!(
        cmdline.value_of("systemd.volatile"),
        Some("state" | "overlay")
    ) && config.var.mount.is_none()
        && !config.var.transient
    {
        debug(format_args!(
            "leaving /var setup to systemd for volatile state"
        ));
        config.var.mount = Some(MountType::None);
    }

    Ok(config)
}

/// Open a directory as an O_PATH anchor for subsequent mount operations.
fn open_dir(dir: impl AsFd, name: impl AsRef<Path>) -> Result<OwnedFd> {
    let name = name.as_ref();
    openat(
        dir,
        name,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .with_context(|| format!("open directory {name:?}"))
}

/// Create and open an overlay state directory, retaining existing permissions.
fn ensure_dir(dir: impl AsFd, name: &str, mode: Mode) -> Result<OwnedFd> {
    match mkdirat(&dir, name, mode) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(error) => return Err(error).with_context(|| format!("create directory {name}")),
    }

    open_dir(dir, name)
}

/// Return a detached bind clone of a directory, or of dir itself for an empty
/// name. Its mount flags can be changed independently of the original mount.
fn clone_mount(dir: impl AsFd, name: &str) -> Result<OwnedFd> {
    Ok(open_tree(
        dir,
        name,
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_EMPTY_PATH,
    )?)
}

/// Make only the exposed bind mount read-only; the original mount must stay
/// writable for deployment state. Preserve its security and atime flags.
fn readonly_bind(path: &str) -> Result<()> {
    let existing = statvfs(path)?.f_flag;
    let mut flags = MountFlags::BIND | MountFlags::RDONLY;

    for (stat_flag, mount_flag) in [
        (StatVfsMountFlags::NOSUID, MountFlags::NOSUID),
        (StatVfsMountFlags::NODEV, MountFlags::NODEV),
        (StatVfsMountFlags::NOEXEC, MountFlags::NOEXEC),
        (StatVfsMountFlags::NOATIME, MountFlags::NOATIME),
        (StatVfsMountFlags::NODIRATIME, MountFlags::NODIRATIME),
        (StatVfsMountFlags::RELATIME, MountFlags::RELATIME),
        // Linux ST_NOSYMFOLLOW; rustix 1.1.6 does not name this statvfs flag.
        (
            StatVfsMountFlags::from_bits_retain(0x2000),
            MountFlags::NOSYMFOLLOW,
        ),
    ] {
        if existing.contains(stat_flag) {
            flags |= mount_flag;
        }
    }

    mount_remount(path, flags, "").with_context(|| format!("make {path} bind mount read-only"))?;
    Ok(())
}

/// Build a detached writable overlay over base, with upper/work under state.
/// The source name identifies the overlay to tools after switch-root.
fn overlay(base: impl AsFd, state: impl AsFd, source: &str) -> Result<OwnedFd> {
    // The merged view inherits upperdir's mode; it must be accessible to
    // non-root processes. The kernel-private workdir stays 0700.
    let upper = ensure_dir(&state, "upper", 0o755.into())?;
    let work = ensure_dir(&state, "work", 0o700.into())?;

    let fs = FsHandle::open("overlay")?;
    fsconfig_set_string(&fs, "source", source)?;
    overlayfs_set_fd(fs.as_fd(), "workdir", work.as_fd())?;
    overlayfs_set_fd(fs.as_fd(), "upperdir", upper.as_fd())?;
    overlayfs_set_lower_and_data_fds(&fs, base, &[])?;
    fsconfig_create(&fs)?;

    Ok(fsmount(
        &fs,
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::empty(),
    )?)
}

/// Back an overlay's writable state with a fresh tmpfs so changes last only
/// for this boot. prepare_mount makes it usable on kernels before Linux 6.15.
fn transient_overlay(base: impl AsFd, source: &str) -> Result<OwnedFd> {
    let fs = FsHandle::open("tmpfs")?;
    fsconfig_create(&fs)?;
    let tmpfs = fsmount(&fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;

    let state = prepare_mount(tmpfs)?;
    overlay(base, state, source)
}

/// Apply /etc or /var policy: retain the image directory, bind deployment state,
/// or layer persistent/transient changes over the image. Bind is the default.
fn mount_subdir(root: impl AsFd, state: impl AsFd, name: &str, config: MountConfig) -> Result<()> {
    let kind = config.mount.unwrap_or(if config.transient {
        MountType::Transient
    } else {
        MountType::Bind
    });
    debug(format_args!("setting up /{name}: {kind:?}"));

    let mounted = match kind {
        MountType::None => return Ok(()),
        MountType::Bind => clone_mount(&state, name)?,
        MountType::Overlay => overlay(open_dir(&root, name)?, open_dir(&state, name)?, "overlay")?,
        MountType::Transient => transient_overlay(open_dir(&root, name)?, "transient")?,
    };

    mount_at(mounted, root, name).with_context(|| format!("mount /{name}"))?;
    Ok(())
}

/// Try parsed candidates in order and return a detached read-only composefs
/// mount plus its digest for selecting deployment state. Only a missing image
/// permits fallback; verification or mount failures must stop boot.
fn mount_image(sysroot: impl AsFd, images: &[ComposefsImage]) -> Result<(OwnedFd, String)> {
    // Upgrade old repositories lacking meta.json, but never let mutable
    // repository metadata override the fs-verity policy in the kernel cmdline.
    debug(format_args!(
        "opening composefs repository /sysroot/composefs; fs-verity required={}",
        !images[0].insecure
    ));
    let (mut repo, _) = Repository::<Sha512HashValue>::open_upgrade(sysroot, "composefs")?;
    repo.set_integrity_policy(IntegrityPolicy::Explicit(if images[0].insecure {
        VerityState::Disabled
    } else {
        VerityState::Enabled
    }))?;

    for (index, image) in images.iter().enumerate() {
        debug(format_args!("mounting composefs candidate {}", index + 1));
        match repo.mount_digest(&image.digest, &MountOptions::default()) {
            Ok(root) => {
                debug(format_args!("composefs candidate {} mounted", index + 1));
                return Ok((root, image.digest.to_hex()));
            }
            // Only missing images allow fallback. Integrity or mount failures
            // must stop boot, even if another candidate exists.
            Err(error) if error.downcast_ref::<ImageNotFound>().is_some() => {
                debug(format_args!("composefs candidate {} not found", index + 1));
                continue;
            }
            Err(error) => return Err(error).context("mount composefs image"),
        }
    }

    bail!(
        "no composefs image found (tried: {})",
        images
            .iter()
            .map(|image| image.digest.to_hex())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Replace /sysroot with the composed root and attach its /etc and /var state.
/// Keep the physical repository visible through a read-only /sysroot clone,
/// while state mounts retain access to the writable original filesystem.
pub(crate) fn setup_root(cmdline: &Cmdline<'_>, images: &[ComposefsImage]) -> Result<()> {
    let config = read_config(cmdline)?;
    let sysroot = open_dir(CWD, "/sysroot")?;

    let (root, digest) =
        mount_image(&sysroot, images).context("mount image from /sysroot/composefs")?;
    let physical_root = clone_mount(&sysroot, "")?;

    // Mount before adding submounts for Linux 6.12 compatibility. Composefs
    // already creates a read-only mount with MountOptions::default().
    debug(format_args!(
        "attaching composed root at /sysroot, transient root={}",
        config.root.transient
    ));
    mount_at(&root, CWD, "/sysroot").context("attach composefs root")?;

    if config.root.transient {
        let overlay = transient_overlay(&root, &format!("transient:composefs={digest}"))?;
        mount_at(overlay, CWD, "/sysroot").context("attach transient root overlay")?;
    }

    // Reopen the visible tree so submounts land in the merged view when a
    // transient root overlay covers the composefs lower layer.
    let visible_root = open_dir(CWD, "/sysroot")?;
    match mount_at(physical_root, &visible_root, "sysroot") {
        Ok(()) => {
            debug(format_args!(
                "exposing physical root read-only at /sysroot/sysroot"
            ));
            readonly_bind("/sysroot/sysroot")?;
        }
        Err(Errno::NOENT) => debug(format_args!(
            "image has no /sysroot directory; physical root clone is not exposed"
        )),
        Err(error) => return Err(error).context("attach physical sysroot"),
    }

    let deployment = format!("state/deploy/{digest}");
    debug(format_args!("using deployment state {deployment}"));
    let state = open_dir(&sysroot, deployment)?;
    mount_subdir(&visible_root, &state, "etc", config.etc).context("configure /etc state")?;
    mount_subdir(&visible_root, &state, "var", config.var).context("configure /var state")?;
    Ok(())
}
