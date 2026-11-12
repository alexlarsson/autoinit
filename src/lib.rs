//! Minimal initramfs for a local, native composefs bootc deployment.
#![forbid(unsafe_code)]

use anyhow::{Context, Result, bail, ensure};
use linux_kernel_cmdline::utf8::Cmdline;
use rustix::{
    fs::{CWD, Mode, OFlags, openat},
    io::Errno,
    mount::{MountFlags, MountPropagationFlags, mount, mount_change, mount_move},
    rand::{GetRandomFlags, getrandom},
    stdio::{dup2_stderr, dup2_stdin, dup2_stdout},
    system::{RebootCommand, finit_module, reboot},
};
use std::ffi::{CStr, CString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);
static CONSOLE_ENABLED: AtomicBool = AtomicBool::new(true);
static KMSG: OnceLock<File> = OnceLock::new();

mod block;
mod extras;
mod setup_root;

/// Mount the kernel API filesystems and shared /run needed during early boot,
/// then connect standard streams to the console for diagnostics.
fn mount_apis() -> Result<()> {
    mount_change(
        "/",
        MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
    )
    .context("make root mounts private")?;

    for (source, path, kind, flags, options) in [
        (
            "proc",
            "/proc",
            "proc",
            MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NODEV,
            c"",
        ),
        (
            "sysfs",
            "/sys",
            "sysfs",
            MountFlags::NOSUID | MountFlags::NOEXEC | MountFlags::NODEV,
            c"",
        ),
        (
            "devtmpfs",
            "/dev",
            "devtmpfs",
            MountFlags::NOSUID | MountFlags::STRICTATIME,
            c"mode=0755,size=4m",
        ),
        (
            "tmpfs",
            "/run",
            "tmpfs",
            MountFlags::NOSUID | MountFlags::NODEV | MountFlags::STRICTATIME,
            c"mode=0755,size=20%,nr_inodes=800k",
        ),
    ] {
        fs::create_dir_all(path).with_context(|| format!("create mountpoint {path}"))?;
        mount(source, path, kind, flags, options)
            .with_context(|| format!("mount {kind} on {path}"))?;
    }
    fs::create_dir_all("/sysroot").context("create /sysroot")?;

    // Keep the descriptor across /dev moves and chroot; File opens close-on-exec.
    if let Ok(kmsg) = OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = KMSG.set(kmsg);
    }

    // Connect standard streams for console diagnostics and the final init.
    let console = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/console")
        .context("open /dev/console")?;
    dup2_stdin(&console).context("redirect stdin to console")?;
    dup2_stdout(&console).context("redirect stdout to console")?;
    dup2_stderr(&console).context("redirect stderr to console")?;
    Ok(())
}

/// Send boot diagnostics to the kernel ring buffer and, unless quiet, the console.
fn log(message: impl std::fmt::Display) {
    if CONSOLE_ENABLED.load(Ordering::Relaxed) {
        eprintln!("autoinit: {message}");
    }

    if let Some(mut kmsg) = KMSG.get() {
        // Each write to /dev/kmsg is a complete record, including its prefix.
        let _ = kmsg.write_all(format!("<6>autoinit: {message}\n").as_bytes());
    }
}

/// Format verbose diagnostics only when requested. Callers use format_args!
/// so disabled messages need no allocation or formatting work.
fn debug(message: impl std::fmt::Display) {
    if DEBUG_ENABLED.load(Ordering::Relaxed) {
        log(format_args!("debug: {message}"));
    }
}

/// Read module paths in preload order. The builder already uses canonical
/// kernel module names as the file stems, so no normalization is needed here.
fn parse_module_manifest(manifest: &str) -> Result<Vec<(&Path, &str)>> {
    let mut modules = Vec::new();
    for line in manifest.lines().filter(|line| !line.is_empty()) {
        let path = Path::new(line);
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .with_context(|| format!("module name in manifest path {path:?}"))?;
        modules.push((path, name));
    }

    Ok(modules)
}

/// Collect this module's name.option=value arguments for finit_module, quoting
/// values so embedded whitespace survives the kernel's module-option parser.
fn parse_module_options(cmdline: &Cmdline<'_>, name: &str) -> Result<CString> {
    let prefix = format!("{name}.");
    let mut options = Vec::new();

    for arg in cmdline.iter() {
        let key = arg.key().replace('-', "_");
        if let Some(option) = key.strip_prefix(&prefix)
            && let Some(v) = arg.value()
        {
            // Module arguments use their own quoting for whitespace.
            if v.contains(['"', '\0']) {
                log(format!("ignoring unrepresentable module argument {key}"));
                continue;
            }
            options.push(format!("{option}=\"{v}\""));
        }
    }

    CString::new(options.join(" ")).context("encode module options")
}

/// Insert a module without stripping its signature; already-loaded modules are OK.
fn load_module(path: &Path, name: &str, options: &CStr) -> Result<()> {
    debug(format_args!(
        "loading module {name} from {path:?}, options {options:?}"
    ));
    let file = File::open(path).with_context(|| format!("open module {path:?}"))?;

    // finit_module preserves appended kernel signatures, including under lockdown.
    match finit_module(&file, options, 0) {
        Ok(()) => {
            debug(format_args!("module {name} loaded"));
            Ok(())
        }
        Err(Errno::EXIST) => {
            debug(format_args!("module {name} already loaded"));
            Ok(())
        }
        Err(error) => Err(error).with_context(|| format!("load module {name}")),
    }
}

/// Report requested modules that the initrd did not load. Preloading is best
/// effort across hardware variants; root discovery determines bootability.
fn warn_missing_modules(cmdline: &Cmdline<'_>) {
    // Cmdline treats '-' and '_' in parameter names as equivalent.
    for key in ["rd.modules-load", "modules-load"] {
        if let Some(names) = cmdline.value_of(key) {
            for name in names.split(',').filter(|s| !s.is_empty()) {
                if !Path::new("/sys/module")
                    .join(name.replace('-', "_"))
                    .exists()
                {
                    log(format!("requested module {name} is absent"));
                }
            }
        }
    }
}

/// Load the builder's precomputed dependency order with per-module options.
/// There is no udev or modprobe here to discover additional modules at runtime.
fn load_modules(cmdline: &Cmdline<'_>) -> Result<()> {
    let manifest =
        fs::read_to_string("/etc/autoinit.modules").context("read /etc/autoinit.modules")?;

    let modules = parse_module_manifest(&manifest)?;
    debug(format_args!("preloading {} modules", modules.len()));

    for (path, name) in modules {
        let attempt = parse_module_options(cmdline, name)
            .and_then(|options| load_module(path, name, &options));
        if let Err(error) = attempt {
            log(format!("could not preload module {name}: {error:#}"));
        }
    }

    warn_missing_modules(cmdline);
    Ok(())
}

#[derive(Debug)]
struct RootMountConfig<'a> {
    spec: &'a str,
    kind: &'a str,
    options: &'a str,
    timeout: Duration,
}

/// Resolve root mount settings, giving automotive overrides precedence over
/// generic kernel arguments. The returned strings borrow the command line.
fn parse_root_mount_config<'a>(cmdline: &'a Cmdline<'_>) -> Result<RootMountConfig<'a>> {
    let spec = cmdline
        .value_of("mount-sysroot.root")
        .or_else(|| cmdline.value_of("root"))
        .context("missing root= or mount-sysroot.root=")?;

    let kind = cmdline
        .value_of("mount-sysroot.rootfstype")
        .or_else(|| cmdline.value_of("rootfstype"))
        .unwrap_or("ext4");

    let ro = cmdline.find("mount-sysroot.ro").is_some()
        || (cmdline.find("mount-sysroot.rw").is_none() && cmdline.find("ro").is_some());
    let options = cmdline.value_of("rootflags").unwrap_or_default();
    ensure!(
        !ro && !options.split(',').any(|option| option == "ro"),
        "native composefs requires a writable physical sysroot for /etc, /var and machine identity; use rw or mount-sysroot.rw and remove rootflags=ro"
    );

    let timeout: u64 = cmdline
        .value_of("autoinit.root-timeout")
        .unwrap_or("10")
        .parse()
        .context("invalid autoinit.root-timeout")?;

    Ok(RootMountConfig {
        spec,
        kind,
        options,
        timeout: Duration::from_secs(timeout),
    })
}

/// Mount the physical root at /sysroot, retrying discovery and mount failures
/// until the timeout because disk probing may still be in progress.
fn mount_root(config: &RootMountConfig<'_>) -> Result<()> {
    debug(format_args!(
        "physical root mount configuration: {config:?}"
    ));
    let options = CString::new(config.options).context("invalid root mount options")?;
    let start = Instant::now();
    let mut next_report = Duration::ZERO;

    loop {
        let elapsed = start.elapsed();
        let report = elapsed >= next_report;
        if report {
            debug(format_args!("looking up root {}", config.spec));
            next_report = elapsed + Duration::from_secs(1);
        }

        let attempt = block::resolve(config.spec).and_then(|device| {
            if report {
                debug(format_args!(
                    "root {} resolved to {device:?}; mounting on /sysroot",
                    config.spec
                ));
            }
            mount(
                &device,
                "/sysroot",
                config.kind,
                MountFlags::empty(),
                options.as_c_str(),
            )
            .with_context(|| format!("mount {device:?} on /sysroot ({})", config.kind))
        });

        match attempt {
            Ok(()) => break,
            Err(error) if start.elapsed() >= config.timeout => {
                return Err(error).with_context(|| {
                    format!(
                        "waiting up to {}s for sysroot {} ({})",
                        config.timeout.as_secs(),
                        config.spec,
                        config.kind
                    )
                });
            }
            Err(error) => {
                if report {
                    debug(format_args!("waiting for root {}: {error:#}", config.spec));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    log(format!(
        "physical sysroot mounted ({:.3}s)",
        start.elapsed().as_secs_f64()
    ));
    Ok(())
}

fn valid_machine_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && id.bytes().any(|b| b != b'0')
}

/// Generate a machine ID from initialized kernel entropy and persist it with
/// an atomic rename. Sync both file and directory before continuing boot.
fn create_persistent_machine_id(path: &Path) -> Result<String> {
    debug(format_args!(
        "creating persistent machine identity at {path:?}"
    ));
    let mut bytes = [0u8; 16];
    let mut offset = 0;
    while offset < bytes.len() {
        let n = match getrandom(&mut bytes[offset..], GetRandomFlags::empty()) {
            Ok(n) => n,
            Err(Errno::INTR) => continue,
            Err(error) => return Err(error).context("getrandom for machine ID"),
        };
        ensure!(n > 0, "getrandom made no progress");
        offset += n;
    }

    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let id = uuid::Uuid::from_bytes(bytes).simple().to_string();

    let temporary = path.with_file_name(".machine-id.autoinit-new");
    let mut f = File::from(
        openat(
            CWD,
            &temporary,
            OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::RGRP | Mode::ROTH,
        )
        .with_context(|| format!("create temporary machine ID {temporary:?}"))?,
    );
    writeln!(f, "{id}").with_context(|| format!("write machine ID to {temporary:?}"))?;
    f.sync_all()
        .with_context(|| format!("sync machine ID {temporary:?}"))?;

    fs::rename(&temporary, path)
        .with_context(|| format!("rename machine ID {temporary:?} to {path:?}"))?;
    let parent = path.parent().context("machine ID parent directory")?;
    File::open(parent)
        .with_context(|| format!("open machine ID directory {parent:?}"))?
        .sync_all()
        .with_context(|| format!("sync machine ID directory {parent:?}"))?;
    debug(format_args!(
        "persistent machine identity saved at {path:?}"
    ));
    Ok(id)
}

/// Recover missing or corrupt persisted identities, including non-UTF-8 data
/// left by an interrupted older writer. Other read failures retain their cause.
fn read_persistent_machine_id(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(id) if valid_machine_id(id.trim()) => {
            debug(format_args!(
                "reusing persistent machine identity from {path:?}"
            ));
            return Ok(id.trim().to_owned());
        }
        Ok(_) => log(format!(
            "invalid persistent machine ID {path:?}; regenerating"
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            log(format!(
                "non-UTF-8 persistent machine ID {path:?}; regenerating"
            ));
        }
        Err(error) => return Err(error).with_context(|| format!("read machine ID {path:?}")),
    }

    create_persistent_machine_id(path)
}

/// Reuse or create the persistent identity before composefs hides physical root,
/// and publish it in /run for the real systemd to consume with transient /etc.
fn machine_id() -> Result<()> {
    let path = Path::new("/sysroot/state/.machine-id");
    let id = read_persistent_machine_id(path)?;

    // systemd reads /run/machine-id on its initial startup too. No prior
    // systemd manager exists to import @initrd credentials in this boot path.
    fs::write("/run/machine-id", format!("{id}\n")).context("write /run/machine-id")?;
    fs::create_dir_all("/run/credentials/@initrd").context("create /run/credentials/@initrd")?;
    fs::write("/run/credentials/@initrd/system.machine_id", id)
        .context("write /run/credentials/@initrd/system.machine_id")?;
    debug(format_args!(
        "published machine identity in /run for systemd"
    ));
    Ok(())
}

/// Free initramfs contents without following symlinks or crossing devices.
fn clean_old_root(path: &Path, device: u64) -> Result<()> {
    for entry in fs::read_dir(path).with_context(|| format!("read old root directory {path:?}"))? {
        let path = entry
            .with_context(|| format!("read directory entry in {path:?}"))?
            .path();
        let meta =
            fs::symlink_metadata(&path).with_context(|| format!("stat old root {path:?}"))?;

        if meta.dev() != device {
            continue;
        }

        if meta.is_dir() {
            clean_old_root(&path, device)?;
            fs::remove_dir(&path).with_context(|| format!("remove old root directory {path:?}"))?;
        } else {
            fs::remove_file(&path).with_context(|| format!("remove old root file {path:?}"))?;
        }
    }
    Ok(())
}

/// Preserve the API mounts and /run, free the initramfs, and replace PID 1 with
/// the real init. The composed root must already contain its state mounts.
fn switch_root(init: &str) -> Result<()> {
    ensure!(
        Path::new("/sysroot")
            .metadata()
            .context("stat /sysroot before switch-root")?
            .dev()
            != Path::new("/")
                .metadata()
                .context("stat / before switch-root")?
                .dev(),
        "new root is not a mount"
    );

    for path in ["run", "dev", "proc", "sys"] {
        let dest = format!("/sysroot/{path}");
        ensure!(Path::new(&dest).is_dir(), "new root lacks /{path}");
        debug(format_args!("moving /{path} to {dest}"));
        mount_move(format!("/{path}"), &dest).with_context(|| format!("move /{path} to {dest}"))?;
    }

    // Keep an fd to rootfs while deleting its files to release initrd RAM.
    let old_root = File::open("/").context("open old root /")?;
    std::env::set_current_dir("/sysroot").context("change directory to /sysroot")?;
    debug(format_args!("removing old initramfs files"));
    clean_old_root(
        Path::new("/"),
        old_root.metadata().context("stat old root /")?.dev(),
    )?;

    debug(format_args!(
        "moving composed root onto / and changing root"
    ));
    mount_move(".", "/").context("move composed root onto /")?;
    rustix::process::chroot(".").context("chroot to composed root")?;
    std::env::set_current_dir("/").context("change directory to new /")?;
    drop(old_root);

    // No systemd initrd serialization state is supplied. The real systemd
    // starts fresh, loads the target SELinux policy and runs udev coldplug.
    debug(format_args!("executing final init {init}"));
    let error = Command::new(init)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .env("HOME", "/")
        .env("TERM", "linux")
        .exec();
    Err(error).with_context(|| format!("exec {init}"))
}

/// Prepare drivers and physical storage, establish identity and composefs
/// state mounts, then hand off PID 1 to the installed system.
fn run(fatal_action: &mut RebootCommand) -> Result<()> {
    ensure!(
        std::process::id() == 1,
        "autoinit must run as PID 1 in an initramfs"
    );

    mount_apis()?;
    let start = Instant::now();

    let raw = fs::read_to_string("/proc/cmdline").context("read /proc/cmdline")?;
    let cmdline = Cmdline::from(raw.as_str());
    DEBUG_ENABLED.store(cmdline.find("autoinit.debug").is_some(), Ordering::Relaxed);
    CONSOLE_ENABLED.store(cmdline.find("quiet").is_none(), Ordering::Relaxed);
    log("starting");
    debug(format_args!(
        "verbose logging enabled; API filesystems are mounted"
    ));
    *fatal_action = failure_action(&cmdline);
    let images = setup_root::parse_composefs_images(&cmdline)?;
    let root_mount = parse_root_mount_config(&cmdline)?;

    load_modules(&cmdline)?;
    log(format!(
        "modules loaded ({:.3}s)",
        start.elapsed().as_secs_f64()
    ));

    extras::setup_network();

    mount_root(&root_mount)?;

    extras::prepare_root();

    if cmdline.value_of("autoinit.machine-id") != Some("off") {
        machine_id()?;
    } else {
        debug(format_args!("persistent machine identity setup disabled"));
    }

    setup_root::setup_root(&cmdline, &images)?;
    log(format!(
        "switching root ({:.3}s)",
        start.elapsed().as_secs_f64()
    ));

    switch_root(cmdline.value_of("init").unwrap_or("/sbin/init"))?;
    bail!("init returned")
}

/// Select the fatal-error action without requiring any mounted root filesystem.
/// Unknown values retain the automotive fail-safe default of immediate reboot.
fn failure_action(cmdline: &Cmdline<'_>) -> RebootCommand {
    match cmdline.value_of("autoinit.failure-action") {
        Some("halt") => RebootCommand::Halt,
        Some("reboot") | None => RebootCommand::Restart,
        Some(value) => {
            log(format!("unknown failure action {value:?}; using reboot"));
            RebootCommand::Restart
        }
    }
}

/// Run PID 1 initialization and reboot on failure after a short diagnostic delay.
pub fn init() {
    let mut action = RebootCommand::Restart;
    if let Err(error) = run(&mut action) {
        log(format!("FATAL: {error:#}"));

        if std::process::id() == 1 {
            log(format!("failure action {action:?} in 3 seconds"));
            std::thread::sleep(Duration::from_secs(3));
            if let Err(error) = reboot(action) {
                log(format!("failure action failed: {error}; exiting PID 1"));
            }
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_mount_config_defaults_and_overrides() -> Result<()> {
        let cmdline = Cmdline::from("root=/dev/vda1");
        let defaults = parse_root_mount_config(&cmdline)?;

        assert_eq!(defaults.spec, "/dev/vda1");
        assert_eq!(defaults.kind, "ext4");
        assert!(defaults.options.is_empty());
        assert_eq!(defaults.timeout, Duration::from_secs(10));

        let cmdline = Cmdline::from(
            "root=/dev/vda1 mount-sysroot.root=PARTLABEL=root rootfstype=xfs \
             mount-sysroot.rootfstype=ext4 rootflags=noatime autoinit.root-timeout=2",
        );
        let config = parse_root_mount_config(&cmdline)?;

        assert_eq!(config.spec, "PARTLABEL=root");
        assert_eq!(config.kind, "ext4");
        assert_eq!(config.options, "noatime");
        assert_eq!(config.timeout, Duration::from_secs(2));

        assert!(parse_root_mount_config(&Cmdline::from("ro")).is_err());
        assert!(
            parse_root_mount_config(&Cmdline::from(
                "root=/dev/vda1 autoinit.root-timeout=invalid"
            ))
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn root_mount_readonly_precedence() -> Result<()> {
        for (options, writable) in [
            ("rw", true),
            ("ro", false),
            ("rw ro", false),
            ("ro mount-sysroot.rw", true),
            ("rw mount-sysroot.ro", false),
            ("mount-sysroot.rw mount-sysroot.ro", false),
            ("rw rootflags=noatime,ro", false),
            ("rw rootflags=errors=remount-ro", true),
        ] {
            let raw = format!("root=/dev/vda1 {options}");
            let cmdline = Cmdline::from(raw.as_str());
            let result = parse_root_mount_config(&cmdline);
            assert_eq!(result.is_ok(), writable, "{options}");
            if let Err(error) = result {
                assert!(error.to_string().contains("writable physical sysroot"));
            }
        }
        Ok(())
    }

    #[test]
    fn module_options_accept_hyphenated_names_and_skip_bad_values() -> Result<()> {
        let cmdline = Cmdline::from(
            r#"nvme-core.io_timeout=10 nvme_core.admin_timeout=30 other.io_timeout=50 nvme_core.bad=a"b"#,
        );
        assert_eq!(
            parse_module_options(&cmdline, "nvme_core")?.to_bytes(),
            br#"io_timeout="10" admin_timeout="30""#,
        );
        Ok(())
    }

    #[test]
    fn corrupt_machine_ids_are_replaced_and_then_reused() -> Result<()> {
        fs::create_dir_all("build")?;
        let temporary = tempfile::tempdir_in("build")?;
        let path = temporary.path().join(".machine-id");

        for invalid in [b"".as_slice(), b"1234", b"not-an-id", b"\xff"] {
            fs::write(&path, invalid)?;
            let id = read_persistent_machine_id(&path)?;
            assert!(valid_machine_id(&id));
            assert_eq!(fs::read_to_string(&path)?.trim(), id);
            assert_eq!(read_persistent_machine_id(&path)?, id);
            fs::remove_file(&path)?;
        }

        let absent = temporary.path().join("absent/.machine-id");
        let error = read_persistent_machine_id(&absent).unwrap_err();
        assert!(format!("{error:#}").contains("absent/.machine-id.autoinit-new"));
        Ok(())
    }

    #[test]
    fn cleanup_does_not_follow_symlinks() -> Result<()> {
        fs::create_dir_all("build")?;
        let temp = tempfile::tempdir_in("build")?;

        let old = temp.path().join("old");
        fs::create_dir_all(old.join("usr/lib"))?;
        fs::write(old.join("usr/lib/module.ko"), b"module")?;

        let outside = temp.path().join("keep");
        fs::create_dir(&outside)?;
        fs::write(outside.join("file"), b"must survive")?;
        std::os::unix::fs::symlink(outside.canonicalize()?, old.join("link"))?;

        clean_old_root(&old, old.metadata()?.dev())?;

        ensure!(fs::read_dir(&old)?.next().is_none(), "old files remain");
        ensure!(
            fs::read(outside.join("file"))? == b"must survive",
            "followed symlink"
        );
        Ok(())
    }

    #[test]
    fn machine_id_validation() {
        assert!(valid_machine_id("1234567890abcdef1234567890abcdef"));
        for bad in [
            "",
            "00000000000000000000000000000000",
            "1234",
            "gggggggggggggggggggggggggggggggg",
        ] {
            assert!(!valid_machine_id(bad));
        }
    }
}
