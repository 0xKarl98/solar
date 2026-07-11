use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use tempfile::TempDir;

pub(crate) const SOLADY_REVISION: &str = "ab96a830e705de13e0f58cfaefadab4ac8257655";
pub(crate) const SOLIDITY_FILE_COUNT: usize = 114;
pub(crate) const SOLIDITY_LINE_COUNT: usize = 53_156;
pub(crate) const SOLIDITY_BYTE_COUNT: usize = 2_317_649;
pub(crate) const STARTUP_MARKER: &str = "solar_bench_startup";
pub(crate) const CROSS_FILE_MARKER: &str = "solar_bench_cross_file";
pub(crate) const FULL_MUL_DIV_CALL: &str =
    "FixedPointMathLib.fullMulDiv(assets, supply, totalAssets())";
pub(crate) const FULL_MUL_DIV_NAME: &str = "fullMulDiv";
pub(crate) const FULL_MUL_DIV_DECLARATION: &str = "function fullMulDiv(";

const ERC4626_PATH: &str = "src/tokens/ERC4626.sol";
const FIXED_POINT_MATH_LIB_PATH: &str = "src/utils/FixedPointMathLib.sol";
const REQUIRED_PATHS: [&str; 3] = ["foundry.toml", ERC4626_PATH, FIXED_POINT_MATH_LIB_PATH];
const IGNORED_DIRECTORIES: [&str; 5] = [".git", "out", "cache", "broadcast", "node_modules"];

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FixtureMetadata {
    pub(crate) revision: String,
    pub(crate) source_file_count: usize,
    pub(crate) source_line_count: usize,
    pub(crate) source_byte_count: usize,
}

pub(crate) struct Project {
    root: PathBuf,
    metadata: FixtureMetadata,
}

impl Project {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let root = path
            .canonicalize()
            .with_context(|| format!("project `{}` does not exist", path.display()))?;
        validate_repository_state(&root, SOLADY_REVISION)?;
        for relative in REQUIRED_PATHS {
            if !root.join(relative).is_file() {
                bail!("Solady checkout is missing `{relative}`")
            }
        }

        let erc4626 = fs::read_to_string(root.join(ERC4626_PATH))?;
        ensure_unique(&erc4626, FULL_MUL_DIV_CALL, ERC4626_PATH)?;
        let fixed_point = fs::read_to_string(root.join(FIXED_POINT_MATH_LIB_PATH))?;
        ensure_unique(&fixed_point, FULL_MUL_DIV_DECLARATION, FIXED_POINT_MATH_LIB_PATH)?;

        let metadata = source_metadata(&root.join("src"))?;
        if metadata.source_file_count != SOLIDITY_FILE_COUNT
            || metadata.source_line_count != SOLIDITY_LINE_COUNT
            || metadata.source_byte_count != SOLIDITY_BYTE_COUNT
        {
            bail!(
                "unexpected Solady source shape: expected {SOLIDITY_FILE_COUNT} files, {SOLIDITY_LINE_COUNT} lines, and {SOLIDITY_BYTE_COUNT} bytes, found {} files, {} lines, and {} bytes",
                metadata.source_file_count,
                metadata.source_line_count,
                metadata.source_byte_count,
            )
        }

        Ok(Self { root, metadata })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn metadata(&self) -> &FixtureMetadata {
        &self.metadata
    }
}

pub(crate) struct Fixture {
    root: TempDir,
}

impl Fixture {
    pub(crate) fn copy_from(project: &Project) -> Result<Self> {
        let root = tempfile::tempdir()?;
        copy_project(project.root(), root.path())?;
        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.path()
    }

    pub(crate) fn erc4626_path(&self) -> PathBuf {
        self.root().join(ERC4626_PATH)
    }

    pub(crate) fn fixed_point_math_lib_path(&self) -> PathBuf {
        self.root().join(FIXED_POINT_MATH_LIB_PATH)
    }
}

fn validate_repository_state(root: &Path, expected_revision: &str) -> Result<()> {
    let revision = git_output(root, &["rev-parse", "HEAD"])?;
    if revision != expected_revision {
        bail!("Solady checkout must be at `{expected_revision}`, found `{revision}`")
    }
    let status = git_output(root, &["status", "--porcelain", "--untracked-files=normal"])?;
    if !status.is_empty() {
        bail!("Solady checkout must have a clean working tree")
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("failed to run Git in `{}`", root.display()))?;
    if !output.status.success() {
        bail!(
            "Git command failed in `{}`: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_unique(contents: &str, anchor: &str, path: &str) -> Result<()> {
    let count = contents.match_indices(anchor).count();
    if count != 1 {
        bail!("expected `{anchor}` exactly once in `{path}`, found {count}")
    }
    Ok(())
}

fn source_metadata(source_root: &Path) -> Result<FixtureMetadata> {
    let mut paths = Vec::new();
    collect_solidity_files(source_root, &mut paths)?;
    paths.sort();

    let mut source_line_count = 0;
    let mut source_byte_count = 0;
    for path in &paths {
        let contents = fs::read(path)?;
        source_line_count += contents.iter().filter(|byte| **byte == b'\n').count();
        source_byte_count += contents.len();
    }

    Ok(FixtureMetadata {
        revision: SOLADY_REVISION.into(),
        source_file_count: paths.len(),
        source_line_count,
        source_byte_count,
    })
}

fn collect_solidity_files(path: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_solidity_files(&path, paths)?;
        } else if path.extension() == Some(OsStr::new("sol")) {
            paths.push(path);
        }
    }
    Ok(())
}

fn copy_project(source: &Path, destination: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_name = entry.file_name();
        if IGNORED_DIRECTORIES.iter().any(|ignored| file_name == OsStr::new(ignored)) {
            continue;
        }

        let source_path = entry.path();
        let destination_path = destination.join(&file_name);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            fs::create_dir_all(&destination_path)?;
            copy_project(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path)?;
        } else if file_type.is_symlink() {
            bail!("Solady checkout contains unsupported symlink `{}`", source_path.display())
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_validation_rejects_wrong_revision_and_dirty_tree() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        git(root.path(), &["config", "user.email", "bench@example.com"]);
        git(root.path(), &["config", "user.name", "LSP Bench"]);
        fs::write(root.path().join("tracked"), "clean\n").unwrap();
        git(root.path(), &["add", "tracked"]);
        git(root.path(), &["commit", "-qm", "fixture"]);
        let revision = git_output(root.path(), &["rev-parse", "HEAD"]).unwrap();

        assert!(validate_repository_state(root.path(), "wrong").is_err());
        validate_repository_state(root.path(), &revision).unwrap();

        fs::write(root.path().join("untracked"), "dirty\n").unwrap();
        assert!(validate_repository_state(root.path(), &revision).is_err());
    }

    #[test]
    fn project_copy_ignores_repository_and_build_directories() {
        let source = tempfile::tempdir().unwrap();
        fs::create_dir_all(source.path().join("src")).unwrap();
        fs::create_dir_all(source.path().join(".git")).unwrap();
        fs::create_dir_all(source.path().join("out")).unwrap();
        fs::create_dir_all(source.path().join("nested/cache")).unwrap();
        fs::write(source.path().join("src/Test.sol"), "contract Test {}\n").unwrap();
        fs::write(source.path().join(".git/config"), "ignored\n").unwrap();
        fs::write(source.path().join("out/Test.json"), "ignored\n").unwrap();
        fs::write(source.path().join("nested/cache/data"), "ignored\n").unwrap();
        let destination = tempfile::tempdir().unwrap();

        copy_project(source.path(), destination.path()).unwrap();

        assert!(destination.path().join("src/Test.sol").is_file());
        assert!(!destination.path().join(".git").exists());
        assert!(!destination.path().join("out").exists());
        assert!(!destination.path().join("nested/cache").exists());
    }

    fn git(root: &Path, args: &[&str]) {
        assert!(Command::new("git").arg("-C").arg(root).args(args).status().unwrap().success());
    }
}
