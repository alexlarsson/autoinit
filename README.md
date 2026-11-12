# autoinit

A small, statically linked Rust initramfs for local-disk, native composefs
bootc systems. It loads selected kernel modules, mounts the deployment and
its state, and switches to the installed system's init as PID 1.

## Build

Requires Rust 1.89 or newer and make. Cargo fetches Rust dependencies,
including composefs 0.10.0, from crates.io. On DNF-based systems, install the
build dependencies from the RPM spec:

```sh
sudo dnf builddep autoinit.spec
make
sudo make install SKIP_BUILD=1
```

`make` builds two statically linked binaries for the host architecture:
`autoinit` and `autoinit-nmstate`, which embeds nmstate for early networking.
They install in `/usr/libexec`, with the builder in `/usr/bin/mkautoinitrd`.
Installation supports `DESTDIR`, `prefix`, `bindir`, and `libexecdir` overrides.
`SKIP_BUILD=1` installs an already-built binary without invoking Cargo again.

## Create an initrd

Requires Python 3, plus `zstd` when reading zstd-compressed kernel modules.
Use the module directory and setup-root configuration from the target system:

```sh
mkautoinitrd --module nvme \
  --config /path/to/setup-root-conf.toml \
  /path/to/lib/modules/KERNEL_VERSION initrd.img
```

To build from an unpacked image, use `--root`:

```sh
mkautoinitrd --root /path/to/image --module nvme \
  /usr/lib/modules/KERNEL_VERSION initrd.img
```

With `--root`, absolute and relative paths for the module directory, `--config`,
and `--nmstate-file` are interpreted within the image. Absolute symlinks also
resolve within that root. Repart's executable, libraries and definitions come
from the image too. Python and the init binaries stay in the build environment;
`--init` and the output filename always refer to host paths.

Repeat `--module DRIVER` to select storage, filesystem, and early network
drivers. Dependencies are included automatically; no modules are selected by
default. Unavailable requested modules and soft dependencies are skipped with a
warning. Failed module preloads are logged and boot continues to root discovery.
Firmware packaging is left to the caller. The builder needs no root
privileges and creates a reproducible gzip-compressed cpio archive; use
`--compress none` for an uncompressed archive.

The builder selects the installed `/usr/libexec/autoinit` by default, or
`/usr/libexec/autoinit-nmstate` when `--nmstate-file` is given.
Use `--init PATH` to override the selected binary.

When `--config` is omitted, the builder includes
`/usr/lib/composefs/setup-root-conf.toml` from the build system if present.
With `--root`, it uses that path in the image instead.
Regenerate the initrd whenever the image's setup-root configuration changes.

Optional early boot features:

- `--nmstate-file PATH` selects the binary with embedded nmstate and applies
  the supplied network configuration after loading modules, using kernel-only
  mode without verification. The initrd needs only the configuration file.
- `--repart` includes `systemd-repart` and `/usr/lib/repart.d` definitions, and
  runs repart against the physical root after mounting it. If the root partition
  grows, autoinit also grows its ext4 filesystem.
- `--growfs` attempts to grow ext4 to the root device's current size every boot,
  including when an image-writing tool has already enlarged the partition.
  It works independently of `--repart` and needs no additional executable.

For repart, run the builder inside the target system or container with
`systemd-repart` installed. The builder uses `ldd` to include its shared
libraries and loader. Repart definitions must already exist in
`/usr/lib/repart.d` and should describe growth of existing partitions.
Networking and storage helper failures are logged and boot continues.

## Build an RPM

Install `cargo-vendor-filterer`, then create source and dependency archives
and a generated spec:

```sh
make package
rpmbuild -ba target/dist/autoinit.spec --define "_sourcedir $PWD/target/dist"
```

`make package-srpm` creates a source RPM directly. Set `XTASK_FLAGS=--offline`
for either target when dependencies are cached. Packaging uses committed `HEAD`,
excluding uncommitted changes, and derives the version from a release tag or
the commit timestamp and hash. Dependency archives cover GNU/Linux targets.
The RPM build uses those archives offline; no adjacent checkouts are needed.

## Boot

Configure the bootloader to load `initrd.img` with the matching kernel. Keep
the `root=` and `composefs=` or `composefs.digest=` arguments identifying the
installed deployment. Root lookup supports device paths, ext4 UUIDs/labels,
GPT/MBR partition UUIDs, and GPT partition labels.

The initrd uses `state/.machine-id` on the physical root for persistent
identity. Set `autoinit.machine-id=off` if the installed system manages this
itself. Invalid persisted machine IDs are logged and regenerated.
`init=` selects the final init executable (default `/sbin/init`).

The physical sysroot must be writable for deployment state. Use `rw`, or
`mount-sysroot.rw` to override a generic `ro`; read-only requests fail early.
The composed image and the exposed physical `/sysroot` clone remain read-only.

Fatal initrd errors reboot after a three-second diagnostic delay. Set
`autoinit.failure-action=halt` to stop the machine instead while debugging.

Add `autoinit.debug` to the kernel command line for verbose boot diagnostics,
including module loads, root discovery and mount retries, composefs selection,
state mount policy, networking, filesystem growth, and switch-root. Messages
go to the console and kernel log. With `quiet`, messages go only to dmesg.

## License

MIT OR Apache-2.0. Root setup is adapted from bootc's initramfs implementation
and uses the composefs library.
