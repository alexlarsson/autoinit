//! Release packaging adapted from bootc's packaging xtasks.
#![forbid(unsafe_code)]

use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};

struct Package {
    source: PathBuf,
    vendor: PathBuf,
    spec: PathBuf,
}

fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("run {command:?}"))?;
    ensure!(status.success(), "{command:?} exited with {status}");
    Ok(())
}

fn output(command: &mut Command) -> Result<String> {
    let result = command
        .output()
        .with_context(|| format!("run {command:?}"))?;
    ensure!(
        result.status.success(),
        "{command:?}: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).context("command output is not UTF-8")
}

fn git(root: &Path, args: &[&str]) -> Result<String> {
    Ok(output(Command::new("git").current_dir(root).args(args))?
        .trim()
        .to_owned())
}

/// Use release tags verbatim apart from RPM's disallowed hyphen, or generate
/// an increasing snapshot version from the commit's UTC timestamp and hash.
fn version(tag: Option<&str>, timestamp: i64, revision: &str) -> Result<String> {
    let version = if let Some(tag) = tag {
        tag.trim_start_matches('v').replace('-', ".")
    } else {
        let date =
            chrono::DateTime::from_timestamp(timestamp, 0).context("invalid commit timestamp")?;
        let revision = revision.get(..10).context("invalid Git revision")?;
        format!("{}.g{revision}", date.format("%Y%m%d%H%M"))
    };

    ensure!(
        !version.is_empty()
            && version
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._+~^".contains(&c)),
        "unsupported RPM version {version:?}"
    );
    Ok(version)
}

/// Make the vendor configuration refer to the directory created by %autosetup,
/// rather than the temporary archive path emitted by cargo-vendor-filterer.
fn vendor_config(text: &str) -> Result<String> {
    let mut config: toml::Value = toml::from_str(text)?;
    let sources = config
        .get_mut("source")
        .and_then(toml::Value::as_table_mut)
        .context("missing Cargo source configuration")?;
    let vendored = sources
        .get_mut("vendored-sources")
        .and_then(toml::Value::as_table_mut)
        .context("missing vendored source configuration")?;
    vendored.insert("directory".into(), "vendor".into());
    Ok(toml::to_string(&config)?)
}

/// Keep the spec version and source names aligned with the generated archives.
fn update_spec(text: &str, version: &str) -> Result<String> {
    let mut fields = [false; 3];
    let mut lines = Vec::new();
    for line in text.lines() {
        let replacement = if line.starts_with("Version:") {
            fields[0] = true;
            format!("Version:        {version}")
        } else if line.starts_with("Source0:") {
            fields[1] = true;
            format!("Source0:        autoinit-{version}.tar.gz")
        } else if line.starts_with("Source1:") {
            fields[2] = true;
            format!("Source1:        autoinit-{version}-vendor.tar.gz")
        } else {
            line.to_owned()
        };
        lines.push(replacement);
    }
    ensure!(
        fields.iter().all(|field| *field),
        "spec is missing Version, Source0 or Source1"
    );
    Ok(lines.join("\n") + "\n")
}

/// Archive HEAD and resolve its dependencies from that archived tree. This
/// keeps dirty working-tree manifests out of both halves of the release.
fn package(root: &Path, offline: bool) -> Result<Package> {
    let revision = git(root, &["rev-parse", "HEAD"])?;
    let timestamp: i64 = git(root, &["show", "-s", "--format=%ct", &revision])?.parse()?;
    let tag = git(root, &["describe", "--tags", "--exact-match", &revision]).ok();
    let version = version(tag.as_deref(), timestamp, &revision)?;
    let name = format!("autoinit-{version}");
    let destination = root.join("target/dist");
    fs::create_dir_all(&destination)?;
    let temporary = tempfile::tempdir_in(&destination)?;
    let source_tar = temporary.path().join("source.tar");

    run(Command::new("git")
        .current_dir(root)
        .args([
            "archive",
            "--format=tar",
            &format!("--prefix={name}/"),
            "--output",
        ])
        .arg(&source_tar)
        .arg(&revision))?;
    run(Command::new("tar")
        .arg("-xf")
        .arg(&source_tar)
        .arg("-C")
        .arg(temporary.path()))?;

    let tree = temporary.path().join(&name);
    let manifest: toml::Value = toml::from_str(&fs::read_to_string(tree.join("Cargo.toml"))?)?;
    ensure!(
        manifest
            .get("package")
            .and_then(|package| package.get("name"))
            .and_then(toml::Value::as_str)
            == Some("autoinit"),
        "expected an autoinit source tree"
    );
    let lockfile = fs::read(tree.join("Cargo.lock"))?;
    let vendor_name = format!("{name}-vendor.tar.gz");
    let vendor_path = temporary.path().join(&vendor_name);
    let mut vendor = Command::new("cargo");
    vendor
        .current_dir(&tree)
        .env("SOURCE_DATE_EPOCH", timestamp.to_string())
        .args([
            "vendor-filterer",
            "--platform",
            "*-unknown-linux-gnu",
            "--tier",
            "2",
            "--all-features",
            "--versioned-dirs",
            "--prefix",
            "vendor",
            "--format",
            "tar.gz",
        ]);
    if offline {
        vendor.arg("--offline");
    }
    let config = vendor_config(&output(vendor.arg(&vendor_path))?)?;
    ensure!(
        fs::read(tree.join("Cargo.lock"))? == lockfile,
        "vendoring changed Cargo.lock"
    );

    // Append only generated configuration. git archive already supplies the
    // committed files in stable order with the commit's timestamp.
    fs::write(temporary.path().join("vendor-config.toml"), config)?;
    run(Command::new("tar")
        .arg("--append")
        .arg("--file")
        .arg(&source_tar)
        .arg("--directory")
        .arg(temporary.path())
        .args([
            "--owner=0",
            "--group=0",
            "--numeric-owner",
            "--mode=0644",
            &format!("--mtime=@{timestamp}"),
            &format!("--transform=s,^,{name}/.cargo/,"),
            "vendor-config.toml",
        ]))?;

    let source_name = format!("{name}.tar.gz");
    let source_path = temporary.path().join(&source_name);
    run(Command::new("gzip")
        .args(["--no-name", "--stdout"])
        .arg(&source_tar)
        .stdout(Stdio::from(File::create(&source_path)?)))?;
    let spec = update_spec(&fs::read_to_string(tree.join("autoinit.spec"))?, &version)?;
    let spec_path = temporary.path().join("autoinit.spec");
    fs::write(&spec_path, spec)?;

    let result = Package {
        source: destination.join(source_name),
        vendor: destination.join(vendor_name),
        spec: destination.join("autoinit.spec"),
    };
    fs::rename(source_path, &result.source)?;
    fs::rename(vendor_path, &result.vendor)?;
    fs::rename(spec_path, &result.spec)?;
    println!(
        "Source: {}\nVendor: {}\nSpec: {}",
        result.source.display(),
        result.vendor.display(),
        result.spec.display()
    );
    Ok(result)
}

/// Generate an SRPM with all RPM scratch files under target/, without requiring
/// native build dependencies or writing into the user's usual RPM directories.
fn package_srpm(root: &Path, offline: bool) -> Result<()> {
    let package = package(root, offline)?;
    let destination = package.spec.parent().context("package output directory")?;
    let temporary = tempfile::tempdir_in(destination)?;
    run(Command::new("rpmbuild")
        .arg("-bs")
        .args([
            "--define",
            &format!("_topdir {}", temporary.path().display()),
        ])
        .args([
            "--define",
            &format!("_tmppath {}", temporary.path().display()),
        ])
        .args(["--define", &format!("_sourcedir {}", destination.display())])
        .args(["--define", &format!("_srcrpmdir {}", destination.display())])
        .arg(&package.spec))?;
    Ok(())
}

fn main() -> Result<()> {
    let mut action = None;
    let mut offline = false;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "package" | "package-srpm" if action.is_none() => action = Some(arg),
            "--offline" => offline = true,
            "-h" | "--help" => {
                println!("Usage: cargo xtask <package|package-srpm> [--offline]");
                return Ok(());
            }
            _ => bail!("unknown argument {arg:?}; use --help"),
        }
    }

    let root = PathBuf::from(git(Path::new("."), &["rev-parse", "--show-toplevel"])?);
    match action.as_deref() {
        Some("package") => {
            package(&root, offline)?;
        }
        Some("package-srpm") => package_srpm(&root, offline)?,
        _ => bail!("expected package or package-srpm; use --help"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_and_snapshot_versions() -> Result<()> {
        assert_eq!(
            version(Some("v1.2.3-rc.1"), 0, "0123456789abcdef")?,
            "1.2.3.rc.1"
        );
        assert_eq!(
            version(None, 0, "0123456789abcdef")?,
            "197001010000.g0123456789"
        );
        assert!(version(Some("release/1.2.3"), 0, "0123456789abcdef").is_err());
        assert!(version(Some("v"), 0, "0123456789abcdef").is_err());
        Ok(())
    }

    #[test]
    fn vendor_configuration_is_relocatable() -> Result<()> {
        let text = vendor_config(
            "[source.crates-io]\nreplace-with = 'vendored-sources'\n[source.vendored-sources]\ndirectory = '/temporary/archive.tar.gz'\n",
        )?;
        let config: toml::Value = toml::from_str(&text)?;
        assert_eq!(
            config["source"]["vendored-sources"]["directory"].as_str(),
            Some("vendor")
        );
        assert!(vendor_config("[source.crates-io]\nreplace-with = 'missing'\n").is_err());
        Ok(())
    }

    #[test]
    fn generated_spec_matches_archive_names() -> Result<()> {
        let input = "Name: autoinit\nVersion: old\nSource0: old.tar.gz\nSource1: old-vendor.tar.gz\n%prep\n%autosetup -a1\n";
        let spec = update_spec(input, "202601010000.g0123456789")?;
        assert!(spec.contains("Version:        202601010000.g0123456789\n"));
        assert!(spec.contains("Source0:        autoinit-202601010000.g0123456789.tar.gz\n"));
        assert!(spec.contains("Source1:        autoinit-202601010000.g0123456789-vendor.tar.gz\n"));
        assert!(spec.ends_with("%autosetup -a1\n"));
        assert!(update_spec("Name: autoinit\n", "1.0").is_err());
        Ok(())
    }
}
