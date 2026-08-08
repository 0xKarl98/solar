//! Preparation and reproducibility checks for benchmark inputs.

use crate::{
    config::{ArtifactSpec, CompilerSpec, Config, FixtureSpec, ServerSpec, SourceSpec},
    fixture::FixtureSource,
    process::{cgroup_v2_process_tree_available, network_isolation_available},
};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const REQUIRED_SERVERS: [&str; 7] =
    ["solar", "asyncswap", "nomic-foundation", "solc", "wake", "juan-blanco", "qiuxiang"];
const REQUIRED_FIXTURES: [&str; 4] = ["synthetic", "v4-core", "aave-v3-origin", "optimism-bedrock"];
pub(crate) const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct PrepareOptions {
    pub(crate) config: PathBuf,
    pub(crate) servers: BTreeSet<String>,
    pub(crate) fixtures: BTreeSet<String>,
}

pub(crate) struct DoctorOptions {
    pub(crate) config: PathBuf,
    pub(crate) publish: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CheckStatus {
    Pass,
    Unavailable,
    Mismatch,
    Unpinned,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Check {
    pub(crate) kind: String,
    pub(crate) id: String,
    pub(crate) status: CheckStatus,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct DoctorReport {
    pub(crate) publish: bool,
    pub(crate) checks: Vec<Check>,
}

impl DoctorReport {
    fn is_publishable(&self) -> bool {
        self.checks.iter().all(|check| matches!(check.status, CheckStatus::Pass))
    }
}

pub(crate) fn prepare(options: PrepareOptions) -> Result<DoctorReport> {
    let config = Config::load(&options.config)?;
    let manifest_dir = options.config.parent().unwrap_or_else(|| Path::new("."));
    for fixture in config
        .fixtures
        .iter()
        .filter(|fixture| options.fixtures.is_empty() || options.fixtures.contains(&fixture.id))
    {
        if !fixture.root.exists() {
            let source = fixture.source.as_ref().with_context(|| {
                format!("fixture `{}` is missing and has no source checkout", fixture.id)
            })?;
            prepare_checkout(source, &fixture.root)?;
        }
        prepare_submodules(&fixture.root)?;
        prepare_fixture_artifacts(fixture)?;
    }
    for server in config
        .servers
        .iter()
        .filter(|server| options.servers.is_empty() || options.servers.contains(&server.id))
    {
        prepare_server(server, manifest_dir)?;
    }
    doctor(DoctorOptions { config: options.config, publish: false })
}

pub(crate) fn doctor(options: DoctorOptions) -> Result<DoctorReport> {
    let config = Config::load(&options.config)?;
    let manifest_dir = options.config.parent().unwrap_or_else(|| Path::new("."));
    let mut checks = Vec::new();
    validate_inventory(&config, &mut checks);
    for server in &config.servers {
        if let Some(source) = &server.source {
            checks.push(check_source_checkout(
                &server.id,
                source,
                &server_source_root(manifest_dir, &server.id),
            ));
        }
        checks.extend(check_server(server));
    }
    for fixture in &config.fixtures {
        checks.push(check_fixture(fixture));
        if let Some(solc) = &fixture.solc {
            checks.extend(check_compiler("solc", &fixture.id, solc));
        }
        if let Some(foundry) = &fixture.foundry {
            checks.extend(check_compiler("foundry", &fixture.id, foundry));
        }
    }
    if options.publish {
        checks.extend(publish_environment_checks(&options.config));
    }
    let report = DoctorReport { publish: options.publish, checks };
    if options.publish && !report.is_publishable() {
        bail!("benchmark environment is not publishable")
    }
    Ok(report)
}

pub(crate) fn render_doctor(report: &DoctorReport) -> String {
    let mut output = String::from("Kind\tID\tStatus\tDetail\n");
    for check in &report.checks {
        output.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            check.kind,
            check.id,
            match check.status {
                CheckStatus::Pass => "pass",
                CheckStatus::Unavailable => "unavailable",
                CheckStatus::Mismatch => "mismatch",
                CheckStatus::Unpinned => "unpinned",
            },
            check.detail
        ));
    }
    output
}

fn validate_inventory(config: &Config, checks: &mut Vec<Check>) {
    let servers = config.servers.iter().map(|server| server.id.as_str()).collect::<BTreeSet<_>>();
    let fixtures =
        config.fixtures.iter().map(|fixture| fixture.id.as_str()).collect::<BTreeSet<_>>();
    for id in REQUIRED_SERVERS {
        checks.push(inventory_check("server-inventory", id, servers.contains(id)));
    }
    for id in REQUIRED_FIXTURES {
        checks.push(inventory_check("fixture-inventory", id, fixtures.contains(id)));
    }
}

fn inventory_check(kind: &str, id: &str, present: bool) -> Check {
    Check {
        kind: kind.into(),
        id: id.into(),
        status: if present { CheckStatus::Pass } else { CheckStatus::Mismatch },
        detail: if present { "declared".into() } else { "required entry is missing".into() },
    }
}

fn check_source_checkout(id: &str, source: &SourceSpec, root: &Path) -> Check {
    if !root.is_dir() {
        return Check {
            kind: "server-source".into(),
            id: id.into(),
            status: CheckStatus::Unavailable,
            detail: format!("source checkout `{}` was not found", root.display()),
        };
    }
    let actual_revision = match git_output(root, &["rev-parse", "HEAD"]) {
        Ok(revision) => revision,
        Err(error) => {
            return Check {
                kind: "server-source".into(),
                id: id.into(),
                status: CheckStatus::Unavailable,
                detail: format!("{error:#}"),
            };
        }
    };
    if actual_revision != source.revision {
        return Check {
            kind: "server-source".into(),
            id: id.into(),
            status: CheckStatus::Mismatch,
            detail: format!("expected revision {}, found {actual_revision}", source.revision),
        };
    }
    let actual_url = git_output(root, &["remote", "get-url", "origin"]);
    if actual_url.as_ref().ok().map(String::as_str) != Some(source.url.as_str()) {
        return Check {
            kind: "server-source".into(),
            id: id.into(),
            status: CheckStatus::Mismatch,
            detail: format!("expected origin `{}`, found {actual_url:?}", source.url),
        };
    }
    let dirty = git_output(root, &["status", "--porcelain", "--untracked-files=normal"])
        .map_or(true, |status| !status.is_empty());
    Check {
        kind: "server-source".into(),
        id: id.into(),
        status: if dirty { CheckStatus::Mismatch } else { CheckStatus::Pass },
        detail: if dirty { "source checkout is dirty".into() } else { actual_revision },
    }
}

fn check_server(server: &ServerSpec) -> Vec<Check> {
    if !server.enabled {
        return vec![Check {
            kind: "server".into(),
            id: server.id.clone(),
            status: if server.required { CheckStatus::Mismatch } else { CheckStatus::Pass },
            detail: if server.required {
                "required server is disabled".into()
            } else {
                "optional server is disabled".into()
            },
        }];
    }
    let executable = resolve_executable(&server.command);
    if !executable.is_file() {
        return vec![Check {
            kind: "server".into(),
            id: server.id.clone(),
            status: CheckStatus::Unavailable,
            detail: format!("executable `{}` was not found", server.command.display()),
        }];
    }
    let Some(artifact) = &server.artifact else {
        return vec![Check {
            kind: "server".into(),
            id: server.id.clone(),
            status: CheckStatus::Unpinned,
            detail: "artifact digest is not declared".into(),
        }];
    };
    let artifact_check = if artifact.sha256.is_none()
        && server.source.as_ref().is_some_and(|source| is_full_git_revision(&source.revision))
        && artifact.path.exists()
    {
        match sha256_path(&artifact.path) {
            Ok(actual) => Check {
                kind: "server-source-build".into(),
                id: server.id.clone(),
                status: CheckStatus::Pass,
                detail: format!("executed artifact {actual}"),
            },
            Err(error) => Check {
                kind: "server-source-build".into(),
                id: server.id.clone(),
                status: CheckStatus::Unavailable,
                detail: format!("{error:#}"),
            },
        }
    } else {
        check_artifact("server-artifact", &server.id, artifact)
    };
    if !matches!(artifact_check.status, CheckStatus::Pass) {
        return vec![artifact_check];
    }
    let executable_check = match sha256_path(&executable) {
        Ok(actual) => Check {
            kind: "server-executable".into(),
            id: server.id.clone(),
            status: CheckStatus::Pass,
            detail: actual,
        },
        Err(error) => Check {
            kind: "server-executable".into(),
            id: server.id.clone(),
            status: CheckStatus::Unavailable,
            detail: format!("{error:#}"),
        },
    };
    if let Err(error) = verify_server_version(server, &executable, VERSION_PROBE_TIMEOUT) {
        return vec![
            artifact_check,
            executable_check,
            Check {
                kind: "server-version".into(),
                id: server.id.clone(),
                status: CheckStatus::Mismatch,
                detail: format!("{error:#}"),
            },
        ];
    }
    vec![
        artifact_check,
        executable_check,
        Check {
            kind: "server-version".into(),
            id: server.id.clone(),
            status: CheckStatus::Pass,
            detail: server.locked_version.clone().unwrap_or_else(|| "version probe passed".into()),
        },
    ]
}

fn is_full_git_revision(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn check_artifact(kind: &str, id: &str, artifact: &ArtifactSpec) -> Check {
    if !artifact.path.exists() {
        return Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Unavailable,
            detail: format!("artifact `{}` was not found", artifact.path.display()),
        };
    }
    let Some(expected) = artifact.sha256.as_deref() else {
        return Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Unpinned,
            detail: "artifact SHA-256 is not declared".into(),
        };
    };
    match sha256_path(&artifact.path) {
        Ok(actual) if actual == expected => {
            Check { kind: kind.into(), id: id.into(), status: CheckStatus::Pass, detail: actual }
        }
        Ok(actual) => Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Mismatch,
            detail: format!("expected {expected}, found {actual}"),
        },
        Err(error) => Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Unavailable,
            detail: format!("{error:#}"),
        },
    }
}

fn check_compiler(kind: &str, fixture: &str, compiler: &CompilerSpec) -> Vec<Check> {
    let mut checks = Vec::new();
    if let Some(path) = &compiler.native {
        checks.push(check_file_digest(
            &format!("{kind}-native"),
            fixture,
            path,
            compiler.native_sha256.as_deref(),
        ));
    }
    if let Some(path) = &compiler.soljson {
        checks.push(check_file_digest(
            &format!("{kind}-soljson"),
            fixture,
            path,
            compiler.soljson_sha256.as_deref(),
        ));
    }
    if compiler.archive_url.is_some() {
        let archive = compiler_archive_path(compiler);
        checks.push(check_file_digest(
            &format!("{kind}-archive"),
            fixture,
            &archive,
            compiler.archive_sha256.as_deref(),
        ));
    }
    if checks.is_empty() {
        checks.push(Check {
            kind: kind.into(),
            id: fixture.into(),
            status: CheckStatus::Unpinned,
            detail: format!("{} has no declared artifact paths", compiler.version),
        });
    }
    checks
}

fn check_file_digest(kind: &str, id: &str, path: &Path, expected: Option<&str>) -> Check {
    if !path.exists() {
        return Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Unavailable,
            detail: format!("artifact `{}` was not found", path.display()),
        };
    }
    let Some(expected) = expected else {
        return Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Unpinned,
            detail: format!("artifact `{}` has no SHA-256", path.display()),
        };
    };
    match sha256_path(path) {
        Ok(actual) if actual == expected => {
            Check { kind: kind.into(), id: id.into(), status: CheckStatus::Pass, detail: actual }
        }
        Ok(actual) => Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Mismatch,
            detail: format!("expected {expected}, found {actual}"),
        },
        Err(error) => Check {
            kind: kind.into(),
            id: id.into(),
            status: CheckStatus::Unavailable,
            detail: format!("{error:#}"),
        },
    }
}

fn compiler_archive_path(compiler: &CompilerSpec) -> PathBuf {
    compiler
        .native
        .as_deref()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."))
        .join("archive.tar.gz")
}

fn prepare_fixture_artifacts(fixture: &FixtureSpec) -> Result<()> {
    if let Some(solc) = &fixture.solc {
        prepare_compiler("solc", &fixture.id, solc)?;
    }
    if let Some(foundry) = &fixture.foundry {
        prepare_compiler("foundry", &fixture.id, foundry)?;
    }
    Ok(())
}

fn prepare_compiler(kind: &str, fixture: &str, compiler: &CompilerSpec) -> Result<()> {
    if let Some(path) = &compiler.native {
        if let Some(url) = &compiler.native_url {
            download_verified(url, path, compiler.native_sha256.as_deref())?;
            make_executable(path)?;
        } else if !path.exists() {
            bail!("{kind} compiler `{}` for fixture `{fixture}` is missing", path.display())
        }
    }
    if let Some(path) = &compiler.soljson {
        if let Some(url) = &compiler.soljson_url {
            download_verified(url, path, compiler.soljson_sha256.as_deref())?;
        } else if !path.exists() {
            bail!("{kind} soljson `{}` for fixture `{fixture}` is missing", path.display())
        }
    }
    if let Some(url) = &compiler.archive_url {
        let archive = compiler_archive_path(compiler);
        download_verified(url, &archive, compiler.archive_sha256.as_deref())?;
        let parent = archive.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let status = Command::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .args(["--no-same-owner", "--no-same-permissions", "-C"])
            .arg(parent)
            .status()
            .with_context(|| format!("failed to extract {kind} archive `{}`", archive.display()))?;
        if !status.success() {
            bail!("failed to extract {kind} archive `{}`", archive.display())
        }
        if let Some(native) = &compiler.native {
            if !native.exists() {
                bail!(
                    "{kind} archive for fixture `{fixture}` did not produce `{}`",
                    native.display()
                )
            }
            make_executable(native)?;
        }
    }
    Ok(())
}

fn download_verified(url: &str, destination: &Path, expected_sha256: Option<&str>) -> Result<()> {
    if destination.exists() {
        if let Some(expected) = expected_sha256 {
            if sha256_path(destination).is_ok_and(|actual| actual == expected) {
                return Ok(());
            }
            bail!("existing artifact `{}` does not match declared SHA-256", destination.display())
        }
        return Ok(());
    }
    if expected_sha256.is_none() {
        bail!("download `{url}` has no SHA-256 pin")
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = destination.with_extension("download.tmp");
    let status = Command::new("curl")
        .args(["--fail", "--location", "--retry", "3", "--silent", "--show-error"])
        .arg(url)
        .args(["--output"])
        .arg(&temporary)
        .status()
        .with_context(|| format!("failed to download `{url}`"))?;
    if !status.success() {
        let _ = fs::remove_file(&temporary);
        bail!("download `{url}` exited with {status}")
    }
    let actual = sha256_path(&temporary)?;
    if Some(actual.as_str()) != expected_sha256 {
        let _ = fs::remove_file(&temporary);
        bail!("download `{url}` has SHA-256 {actual}, expected {expected_sha256:?}")
    }
    fs::rename(&temporary, destination)?;
    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(crate) fn resolve_executable(path: &Path) -> PathBuf {
    if path.is_absolute() || path.components().count() > 1 {
        return path.to_path_buf();
    }
    let Some(paths) = std::env::var_os("PATH") else { return path.to_path_buf() };
    std::env::split_paths(&paths)
        .map(|directory| directory.join(path))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| path.to_path_buf())
}

pub(crate) fn inspect_version(
    command: &Path,
    spec: &ServerSpec,
    timeout: Duration,
) -> Result<String> {
    if spec.version_args.is_empty() {
        return spec.locked_version.clone().with_context(|| {
            format!("server `{}` has no version command or locked version", spec.id)
        });
    }
    let mut process = Command::new(command);
    process
        .args(&spec.version_args)
        .env_remove("RUST_LOG")
        .env_remove("SOLAR_PROFILE")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &spec.env {
        process.env(key, value);
    }
    let mut child =
        process.spawn().with_context(|| format!("failed to run `{}`", command.display()))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            if !status.success() {
                bail!("version command exited with {status}")
            }
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let value = if stdout.trim().is_empty() { stderr.trim() } else { stdout.trim() };
            if value.is_empty() {
                bail!("version command produced no output")
            }
            return Ok(value.to_owned());
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out waiting for version command")
        }
        thread::sleep(Duration::from_millis(5));
    }
}

pub(crate) fn verify_server_version(
    server: &ServerSpec,
    command: &Path,
    timeout: Duration,
) -> Result<()> {
    let actual = inspect_version(command, server, timeout)?;
    verify_server_version_output(server, &actual)
}

pub(crate) fn verify_server_version_output(server: &ServerSpec, actual: &str) -> Result<()> {
    if let Some(expected) = &server.expected_version
        && actual.trim() != expected.trim()
    {
        bail!("version mismatch: expected `{expected}`, found `{actual}`")
    }
    if let Some(expected) = &server.locked_version
        && !actual
            .split(|character: char| {
                !character.is_ascii_alphanumeric() && character != '.' && character != '-'
            })
            .any(|token| {
                token == expected
                    || token.strip_prefix('v') == Some(expected)
                    || token.starts_with(&format!("{expected}+"))
            })
    {
        bail!("locked version `{expected}` was not found in `{actual}`")
    }
    Ok(())
}

fn check_fixture(fixture: &FixtureSpec) -> Check {
    if !fixture.enabled {
        return Check {
            kind: "fixture".into(),
            id: fixture.id.clone(),
            status: if fixture.required { CheckStatus::Mismatch } else { CheckStatus::Pass },
            detail: if fixture.required {
                "required fixture is disabled".into()
            } else {
                "optional fixture is disabled".into()
            },
        };
    }
    match FixtureSource::open(fixture) {
        Ok(source) => Check {
            kind: "fixture".into(),
            id: fixture.id.clone(),
            status: CheckStatus::Pass,
            detail: format!("{} Solidity files", source.metadata().source_file_count),
        },
        Err(error) => Check {
            kind: "fixture".into(),
            id: fixture.id.clone(),
            status: CheckStatus::Unavailable,
            detail: format!(
                "{} fixture: {error:#}",
                if fixture.required { "required" } else { "optional" }
            ),
        },
    }
}

fn publish_environment_checks(config_path: &Path) -> Vec<Check> {
    let mut checks = vec![Check {
        kind: "environment".into(),
        id: "platform".into(),
        status: if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            CheckStatus::Pass
        } else {
            CheckStatus::Mismatch
        },
        detail: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
    }];
    checks.push(match cgroup_v2_process_tree_available() {
        Ok(path) => Check {
            kind: "environment".into(),
            id: "cgroup-v2-process-tree".into(),
            status: CheckStatus::Pass,
            detail: format!("delegated under `{}`", path.parent().unwrap_or(&path).display()),
        },
        Err(error) => Check {
            kind: "environment".into(),
            id: "cgroup-v2-process-tree".into(),
            status: CheckStatus::Unavailable,
            detail: format!("{error:#}"),
        },
    });
    checks.push(match network_isolation_available() {
        Ok(()) => Check {
            kind: "environment".into(),
            id: "network-namespace".into(),
            status: CheckStatus::Pass,
            detail: "unprivileged network namespace available".into(),
        },
        Err(error) => Check {
            kind: "environment".into(),
            id: "network-namespace".into(),
            status: CheckStatus::Unavailable,
            detail: format!("{error:#}"),
        },
    });
    let root = repository_root(config_path).unwrap_or_else(|| PathBuf::from("."));
    let clean = git_output(&root, &["status", "--porcelain", "--untracked-files=normal"])
        .is_ok_and(|status| status.is_empty());
    checks.push(Check {
        kind: "environment".into(),
        id: "git-clean".into(),
        status: if clean { CheckStatus::Pass } else { CheckStatus::Mismatch },
        detail: root.display().to_string(),
    });
    checks
}

fn prepare_server(server: &ServerSpec, manifest_dir: &Path) -> Result<()> {
    let source_root = server_source_root(manifest_dir, &server.id);
    if let Some(source) = &server.source {
        prepare_checkout(source, &source_root)?;
    }
    let Some(install) = &server.install else { return Ok(()) };
    if install.kind == "none" {
        return Ok(());
    }
    let program = install
        .command
        .as_deref()
        .with_context(|| format!("server `{}` install command is missing", server.id))?;
    let artifact_root = manifest_dir.join("../../target/lsp-bench/servers").join(&server.id);
    fs::create_dir_all(&artifact_root)?;
    let args = install
        .args
        .iter()
        .map(|arg| {
            arg.replace("{source}", &source_root.display().to_string())
                .replace("{target}", &artifact_root.display().to_string())
        })
        .collect::<Vec<_>>();
    let status = Command::new(program)
        .args(args)
        .current_dir(if source_root.is_dir() { &source_root } else { manifest_dir })
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("failed to install server `{}`", server.id))?;
    if !status.success() {
        bail!("server `{}` install command exited with {status}", server.id)
    }
    Ok(())
}

fn server_source_root(manifest_dir: &Path, id: &str) -> PathBuf {
    manifest_dir.join("../../target/lsp-bench/sources/servers").join(id)
}

fn prepare_checkout(source: &SourceSpec, destination: &Path) -> Result<()> {
    if destination.join(".git").is_dir() {
        run_git(destination, &["fetch", "--depth=1", "origin", &source.revision])?;
    } else {
        if destination.exists() {
            bail!(
                "checkout destination `{}` exists but is not a Git repository",
                destination.display()
            )
        }
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        let destination_arg = destination.to_string_lossy().into_owned();
        run_git(
            Path::new("."),
            &["clone", "--filter=blob:none", "--no-checkout", &source.url, &destination_arg],
        )?;
    }
    run_git(destination, &["checkout", "--detach", &source.revision])?;
    let actual = git_output(destination, &["rev-parse", "HEAD"])?;
    if actual != source.revision {
        bail!(
            "checkout `{}` resolved to `{actual}`, expected `{}`",
            destination.display(),
            source.revision
        )
    }
    Ok(())
}

fn prepare_submodules(root: &Path) -> Result<()> {
    if !root.join(".git").exists() {
        return Ok(());
    }
    run_git(root, &["submodule", "sync", "--recursive"])?;
    run_git(root, &["submodule", "update", "--init", "--recursive"])
}

fn run_git(root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("git").arg("-C").arg(root).args(args).status()?;
    if !status.success() {
        bail!("Git command failed in `{}` with {status}", root.display())
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").arg("-C").arg(root).args(args).output()?;
    if !output.status.success() {
        bail!("Git command failed in `{}`", root.display())
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn repository_root(path: &Path) -> Option<PathBuf> {
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    git_output(root, &["rev-parse", "--show-toplevel"]).ok().map(PathBuf::from)
}

pub(crate) fn sha256_path(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    if path.is_file() {
        hash_file(path, &mut hasher)?;
    } else if path.is_dir() {
        let mut files = Vec::new();
        collect_files(path, path, &mut files)?;
        files.sort();
        for relative in files {
            hasher.update(relative.to_string_lossy().as_bytes());
            hasher.update([0]);
            hash_file(&path.join(relative), &mut hasher)?;
        }
    } else {
        bail!("artifact `{}` is not a file or directory", path.display())
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn collect_files(root: &Path, directory: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_files(root, &path, files)?;
        } else if entry.file_type()?.is_file() {
            files.push(path.strip_prefix(root)?.to_path_buf());
        }
    }
    Ok(())
}

fn hash_file(path: &Path, hasher: &mut Sha256) -> Result<()> {
    let mut file = fs::File::open(path)?;
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::collections::BTreeMap;

    #[test]
    fn directory_digest_is_stable_and_path_sensitive() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("a")).unwrap();
        fs::write(root.path().join("a/file"), "value").unwrap();
        let first = sha256_path(root.path()).unwrap();
        let second = sha256_path(root.path()).unwrap();
        assert_eq!(first, second);
        fs::rename(root.path().join("a/file"), root.path().join("file")).unwrap();
        assert_ne!(first, sha256_path(root.path()).unwrap());
    }

    #[test]
    fn locked_server_version_must_appear_in_probe_output() {
        let mut server = server_spec();
        server.locked_version = Some("0.8.36".into());
        assert!(
            verify_server_version_output(
                &server,
                "solc, the solidity compiler commandline interface\nVersion: 0.8.36+commit.8a079791"
            )
            .is_ok()
        );
        assert!(verify_server_version_output(&server, "Version: 0.8.35").is_err());
    }

    #[test]
    fn compiler_artifact_check_rejects_missing_and_mismatched_files() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("solc");
        assert!(matches!(
            check_file_digest("solc-native", "fixture", &path, Some("00".repeat(32).as_str()))
                .status,
            CheckStatus::Unavailable
        ));
        fs::write(&path, "compiler").unwrap();
        assert!(matches!(
            check_file_digest("solc-native", "fixture", &path, Some("00".repeat(32).as_str()))
                .status,
            CheckStatus::Mismatch
        ));
        let digest = sha256_path(&path).unwrap();
        assert!(matches!(
            check_file_digest("solc-native", "fixture", &path, Some(&digest)).status,
            CheckStatus::Pass
        ));
    }

    #[test]
    fn server_source_checkout_must_match_the_locked_revision_and_be_clean() {
        let root = tempfile::tempdir().unwrap();
        run_git(root.path(), &["init"]).unwrap();
        run_git(root.path(), &["remote", "add", "origin", "https://example.invalid/server.git"])
            .unwrap();
        fs::write(root.path().join("source"), "first").unwrap();
        run_git(root.path(), &["add", "source"]).unwrap();
        run_git(
            root.path(),
            &[
                "-c",
                "user.name=lsp-bench",
                "-c",
                "user.email=lsp-bench@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        )
        .unwrap();
        let revision = git_output(root.path(), &["rev-parse", "HEAD"]).unwrap();
        let source = SourceSpec { url: "https://example.invalid/server.git".into(), revision };

        assert!(matches!(
            check_source_checkout("server", &source, root.path()).status,
            CheckStatus::Pass
        ));
        fs::write(root.path().join("source"), "changed").unwrap();
        assert!(matches!(
            check_source_checkout("server", &source, root.path()).status,
            CheckStatus::Mismatch
        ));
    }

    fn server_spec() -> ServerSpec {
        ServerSpec {
            id: "server".into(),
            command: "server".into(),
            args: Vec::new(),
            transport: crate::config::TransportSpec::Stdio,
            version_args: vec!["--version".into()],
            locked_version: None,
            expected_version: None,
            enabled: true,
            env: BTreeMap::new(),
            initialization_options: Value::Null,
            configuration: Value::Null,
            label: None,
            source: None,
            install: None,
            artifact: None,
            required: false,
        }
    }
}
