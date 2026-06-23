use std::path::Path;

use lsp_types::{
    DidChangeWatchedFilesRegistrationOptions, FileSystemWatcher, GlobPattern, Registration,
    RegistrationParams, WatchKind,
    notification::{DidChangeWatchedFiles, Notification},
};

const WATCHED_FILES_REGISTRATION_ID: &str = "solar-watched-files";
const SOLIDITY_SOURCE_EXTENSION: &str = "sol";
const SOLIDITY_SOURCE_GLOB: &str = "**/*.sol";
const FOUNDRY_MANIFEST_FILE_NAME: &str = "foundry.toml";
const FOUNDRY_MANIFEST_GLOB: &str = "**/foundry.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchedFileKind {
    SoliditySource,
    FoundryManifest,
}

pub(crate) fn classify_path(path: &Path) -> Option<WatchedFileKind> {
    match path.file_name().and_then(|name| name.to_str()) {
        Some(FOUNDRY_MANIFEST_FILE_NAME) => Some(WatchedFileKind::FoundryManifest),
        Some(_) if path.extension().is_some_and(|ext| ext == SOLIDITY_SOURCE_EXTENSION) => {
            Some(WatchedFileKind::SoliditySource)
        }
        _ => None,
    }
}

pub(crate) fn did_change_watched_files_registration() -> RegistrationParams {
    RegistrationParams {
        registrations: vec![Registration {
            id: WATCHED_FILES_REGISTRATION_ID.into(),
            method: DidChangeWatchedFiles::METHOD.into(),
            register_options: Some(
                serde_json::to_value(DidChangeWatchedFilesRegistrationOptions {
                    watchers: watched_file_watchers(),
                })
                .expect("watched file registration options should serialize"),
            ),
        }],
    }
}

fn watched_file_watchers() -> Vec<FileSystemWatcher> {
    [SOLIDITY_SOURCE_GLOB, FOUNDRY_MANIFEST_GLOB]
        .into_iter()
        .map(|pattern| FileSystemWatcher {
            glob_pattern: GlobPattern::String(pattern.into()),
            kind: Some(WatchKind::Create | WatchKind::Change | WatchKind::Delete),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use lsp_types::notification;

    use super::*;

    #[test]
    fn classifies_disk_paths_that_match_registered_watchers() {
        assert_eq!(
            classify_path(Path::new("/workspace/src/Counter.sol")),
            Some(WatchedFileKind::SoliditySource)
        );
        assert_eq!(
            classify_path(Path::new("/workspace/foundry.toml")),
            Some(WatchedFileKind::FoundryManifest)
        );
        assert_eq!(classify_path(Path::new("/workspace/README.md")), None);
    }

    #[test]
    fn watched_files_registration_requests_solidity_and_foundry_changes() {
        let params = did_change_watched_files_registration();

        assert_eq!(params.registrations.len(), 1);
        let registration = &params.registrations[0];
        assert_eq!(registration.id, WATCHED_FILES_REGISTRATION_ID);
        assert_eq!(registration.method, notification::DidChangeWatchedFiles::METHOD);

        let options: DidChangeWatchedFilesRegistrationOptions =
            serde_json::from_value(registration.register_options.clone().unwrap()).unwrap();
        assert_eq!(options.watchers, watched_file_watchers());
    }
}
