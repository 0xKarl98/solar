use anyhow::Result;
use std::{
    fs,
    path::{Path, PathBuf},
};
use tempfile::TempDir;

pub(crate) const SOLIDITY_FILE_COUNT: usize = 184;
pub(crate) const STARTUP_MARKER: &str = "bench_startup";

pub(crate) struct Fixture {
    root: TempDir,
}

impl Fixture {
    pub(crate) fn create() -> Result<Self> {
        let root = tempfile::tempdir()?;
        fs::create_dir_all(root.path().join("src/interfaces"))?;
        fs::create_dir_all(root.path().join("src/lib"))?;
        fs::create_dir_all(root.path().join("src/modules"))?;
        fs::write(root.path().join("foundry.toml"), "[profile.default]\nsrc = \"src\"\n")?;
        fs::write(
            root.path().join("src/Owned.sol"),
            "pragma solidity ^0.8.0;\ncontract Owned { address public owner; }\n",
        )?;
        fs::write(
            root.path().join("src/lib/PercentMath.sol"),
            "pragma solidity ^0.8.0;\nlibrary PercentMath { function percent(uint256 value) internal pure returns (uint256) { return value / 100; } }\n",
        )?;
        fs::write(
            root.path().join("src/interfaces/ITreasury.sol"),
            "pragma solidity ^0.8.0;\ninterface ITreasury {}\n",
        )?;

        for module in 0..180 {
            let import = if module == 0 {
                String::new()
            } else {
                format!("import \"./Module{:03}.sol\";\n", module - 1)
            };
            let body = if [20, 60, 100, 140].contains(&module) {
                format!("function benchmark() external pure {{\n// cross_{module:03};\n}}")
            } else {
                format!("function value() external pure returns (uint256) {{ return {module}; }}")
            };
            fs::write(
                root.path().join(format!("src/modules/Module{module:03}.sol")),
                format!(
                    "pragma solidity ^0.8.0;\n{import}contract Module{module:03} {{ {body} }}\n"
                ),
            )?;
        }

        fs::write(
            root.path().join("src/Treasury.sol"),
            format!(
                "pragma solidity ^0.8.0;\n\
                 import \"./Owned.sol\";\n\
                 import \"./interfaces/ITreasury.sol\";\n\
                 import \"./lib/PercentMath.sol\";\n\
                 import \"./modules/Module179.sol\";\n\
                 contract Treasury is Owned, ITreasury {{\n\
                     function benchmark() external pure {{ {STARTUP_MARKER}; }}\n\
                 }}\n"
            ),
        )?;

        Ok(Self { root })
    }

    pub(crate) fn root(&self) -> &Path {
        self.root.path()
    }

    #[cfg(test)]
    fn treasury_contents(&self) -> Result<String> {
        fs::read_to_string(self.treasury_path()).map_err(Into::into)
    }

    pub(crate) fn treasury_path(&self) -> PathBuf {
        self.root().join("src/Treasury.sol")
    }

    pub(crate) fn module_path(&self, module: usize) -> PathBuf {
        self.root().join(format!("src/modules/Module{module:03}.sol"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_fixture_has_stable_shape_and_markers() {
        let fixture = Fixture::create().unwrap();
        let file_count = walk_solidity_files(fixture.root()).unwrap();
        let treasury = fixture.treasury_contents().unwrap();

        assert_eq!(file_count, SOLIDITY_FILE_COUNT);
        assert!(treasury.contains(STARTUP_MARKER));
        for module in [20, 60, 100, 140] {
            let contents = std::fs::read_to_string(
                fixture.root().join(format!("src/modules/Module{module:03}.sol")),
            )
            .unwrap();
            assert!(contents.contains(&format!("cross_{module:03}")));
        }
    }

    fn walk_solidity_files(path: &Path) -> Result<usize> {
        let mut count = 0;
        for entry in std::fs::read_dir(path)? {
            let path = entry?.path();
            if path.is_dir() {
                count += walk_solidity_files(&path)?;
            } else if path.extension().is_some_and(|extension| extension == "sol") {
                count += 1;
            }
        }
        Ok(count)
    }
}
