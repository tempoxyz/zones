//! Offline integrity checks for the vendored Earn benchmark fixtures.

use eyre::{Context as _, ensure, eyre};
use sha2::{Digest as _, Sha256};
use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const EARN_FIXTURE_LOCK: &str = include_str!("../../contrib/bench/earn.lock");
const EARN_FIXTURE_TREE: &str = "specs/ref-impls/test/fixtures/earn";
const DIGEST_DOMAIN: &[u8] = b"zones-earn-vendor-v1\0";

#[derive(Debug, PartialEq, Eq)]
struct EarnFixtureLock<'a> {
    revision: &'a str,
    solidity_files: usize,
    vendor_sha256: &'a str,
}

pub(crate) fn earn_fixture_revision() -> eyre::Result<&'static str> {
    Ok(parse_lock(EARN_FIXTURE_LOCK)?.revision)
}

/// Verify the checked-in vendored sources before fixture deployment touches the network.
pub(crate) fn verify_earn_fixture_lock() -> eyre::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(EARN_FIXTURE_TREE);
    verify_earn_fixture_lock_at(&root, EARN_FIXTURE_LOCK)
}

fn verify_earn_fixture_lock_at(root: &Path, contents: &str) -> eyre::Result<()> {
    let fixture_lock = parse_lock(contents)?;
    let files = solidity_files(root)?;
    ensure!(
        files.len() == fixture_lock.solidity_files,
        "vendored Earn fixture count drifted: expected {}, found {}",
        fixture_lock.solidity_files,
        files.len()
    );

    let actual = fixture_digest(root, &files)?;
    ensure!(
        actual == fixture_lock.vendor_sha256,
        "vendored Earn fixtures drifted from contrib/bench/earn.lock: expected {}, found {}",
        fixture_lock.vendor_sha256,
        actual
    );
    Ok(())
}

fn parse_lock(contents: &str) -> eyre::Result<EarnFixtureLock<'_>> {
    let value = |key: &str| -> eyre::Result<&str> {
        let mut values = contents.lines().filter_map(|line| line.strip_prefix(key));
        let value = values.next().ok_or_else(|| {
            eyre!(
                "Earn fixture lock does not define {}",
                key.trim_end_matches('=')
            )
        })?;
        ensure!(
            values.next().is_none(),
            "Earn fixture lock defines {} more than once",
            key.trim_end_matches('=')
        );
        Ok(value)
    };

    let revision = value("EARN_REV=")?;
    ensure!(
        revision.len() == 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Earn fixture lock contains an invalid EARN_REV"
    );

    let solidity_files = value("EARN_VENDOR_SOLIDITY_FILES=")?
        .parse::<usize>()
        .wrap_err("Earn fixture lock contains an invalid EARN_VENDOR_SOLIDITY_FILES")?;
    ensure!(
        solidity_files > 0,
        "Earn fixture lock must cover at least one Solidity source"
    );

    let vendor_sha256 = value("EARN_VENDOR_SHA256=")?;
    ensure!(
        vendor_sha256.len() == 64
            && vendor_sha256
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "Earn fixture lock contains an invalid EARN_VENDOR_SHA256"
    );

    Ok(EarnFixtureLock {
        revision,
        solidity_files,
        vendor_sha256,
    })
}

fn solidity_files(root: &Path) -> eyre::Result<Vec<String>> {
    let mut files = Vec::new();
    collect_solidity_files(root, root, &mut files)?;
    files.sort_unstable();
    Ok(files)
}

fn collect_solidity_files(
    root: &Path,
    directory: &Path,
    files: &mut Vec<String>,
) -> eyre::Result<()> {
    for entry in fs::read_dir(directory).wrap_err_with(|| {
        format!(
            "failed reading vendored Earn directory {}",
            directory.display()
        )
    })? {
        let entry = entry.wrap_err_with(|| {
            format!(
                "failed reading an entry from vendored Earn directory {}",
                directory.display()
            )
        })?;
        let file_type = entry
            .file_type()
            .wrap_err_with(|| format!("failed reading file type for {}", entry.path().display()))?;
        ensure!(
            !file_type.is_symlink(),
            "vendored Earn fixture tree contains symlink {}",
            entry.path().display()
        );
        if file_type.is_dir() {
            collect_solidity_files(root, &entry.path(), files)?;
        } else if file_type.is_file()
            && entry.path().extension().and_then(|value| value.to_str()) == Some("sol")
        {
            files.push(normalized_relative_path(root, &entry.path())?);
        }
    }
    Ok(())
}

fn normalized_relative_path(root: &Path, path: &Path) -> eyre::Result<String> {
    let relative = path.strip_prefix(root).wrap_err_with(|| {
        format!(
            "vendored Earn source {} is outside {}",
            path.display(),
            root.display()
        )
    })?;
    relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| eyre!("vendored Earn source path is not valid UTF-8")),
            _ => Err(eyre!(
                "vendored Earn source path contains a non-normal component"
            )),
        })
        .collect::<eyre::Result<Vec<_>>>()
        .map(|components| components.join("/"))
}

fn fixture_digest(root: &Path, files: &[String]) -> eyre::Result<String> {
    let mut digest = Sha256::new();
    digest.update(DIGEST_DOMAIN);
    for relative in files {
        let contents = fs::read(root.join(relative))
            .wrap_err_with(|| format!("failed reading vendored Earn source {relative}"))?;
        digest.update((relative.len() as u64).to_be_bytes());
        digest.update(relative.as_bytes());
        digest.update((contents.len() as u64).to_be_bytes());
        digest.update(contents);
    }
    Ok(const_hex::encode(digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_in_earn_fixture_tree_matches_lock() {
        verify_earn_fixture_lock().unwrap();
    }

    #[test]
    fn fixture_lock_requires_complete_unique_fields() {
        assert!(
            parse_lock(
                "EARN_REV=0000000000000000000000000000000000000000\n\
                 EARN_VENDOR_SOLIDITY_FILES=40\n\
                 EARN_VENDOR_SHA256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            )
            .is_ok()
        );
        assert!(
            parse_lock(
                "EARN_REV=0000000000000000000000000000000000000000\n\
                 EARN_VENDOR_SOLIDITY_FILES=40\n"
            )
            .is_err()
        );
        assert!(
            parse_lock(
                "EARN_REV=0000000000000000000000000000000000000000\n\
                 EARN_REV=1111111111111111111111111111111111111111\n\
                 EARN_VENDOR_SOLIDITY_FILES=40\n\
                 EARN_VENDOR_SHA256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
            )
            .is_err()
        );
    }
}
