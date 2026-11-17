use std::path::Path;

use git2::Repository;

pub const REPO_FLAGS: git2::RepositoryOpenFlags = git2::RepositoryOpenFlags::NO_SEARCH;

pub const AVERAGE_DAYS_TO_HASH_EVERYTHING: u64 = 365;

pub const IGNORED_NAMES: [&str; 5] = [
    ".backup_uuids.txt",
    "node_modules",
    "E-Stonks/Cache",
    ".venv",
    ".git",
];

pub fn ignore(absolute_path: &Path, repo: &Option<Repository>) -> Result<bool, anyhow::Error> {
    let file_path = absolute_path.to_path_buf();

    for ignored_name in IGNORED_NAMES {
        if file_path
            .to_str()
            .unwrap_or_default()
            .contains(ignored_name)
        {
            return Ok(true);
        };
    }

    Ok(match repo {
        Some(repo) => repo.status_should_ignore(absolute_path)?,
        None => false,
    })
}

pub fn protect(relative_path: &Path) -> bool {
    relative_path.starts_with(".backup/")
}
