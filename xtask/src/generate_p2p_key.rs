use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use commonware_codec::Encode as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use commonware_math::algebra::Random as _;
use rand::RngExt as _;

const TEMP_FILE_ATTEMPTS: usize = 16;

/// Generate an Ed25519 identity for multi-sequencer P2P communication.
#[derive(Debug, clap::Parser)]
pub(crate) struct GenerateP2pKey {
    /// Destination for the unencrypted hex-encoded private key.
    #[arg(long = "out", short, default_value = "p2p.key", value_name = "PATH")]
    output: PathBuf,

    /// Replace the destination if it already exists.
    #[arg(long, short)]
    force: bool,
}

impl GenerateP2pKey {
    pub(crate) fn run(self) -> eyre::Result<()> {
        let key = PrivateKey::random(rand::rng());
        let encoded_key = format!("{}\n", const_hex::encode_prefixed(key.encode().as_ref()));
        write_key_file(&self.output, self.force, encoded_key.as_bytes())?;

        println!("{}", const_hex::encode_prefixed(key.public_key().as_ref()));
        Ok(())
    }
}

/// Write key material without ever modifying an existing destination inode.
///
/// A forced rotation replaces the destination only after the new key has been
/// fully written and synced. This prevents readers holding a descriptor to a
/// previous key file from observing the replacement key.
fn write_key_file(output: &Path, force: bool, contents: &[u8]) -> eyre::Result<()> {
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));

    if output.file_name().is_none() {
        eyre::bail!(
            "P2P key destination `{}` must name a file",
            output.display()
        );
    }

    match fs::symlink_metadata(output) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                eyre::bail!(
                    "refusing to replace non-regular P2P key destination `{}`",
                    output.display()
                );
            }
            if !force {
                eyre::bail!(
                    "P2P key destination `{}` already exists; use --force to replace it",
                    output.display()
                );
            }
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(eyre::eyre!(
                "failed inspecting P2P key destination `{}`: {err}",
                output.display()
            ));
        }
    }

    let (temporary_path, temporary_file) = create_temporary_key_file(parent, output)?;
    let result =
        write_and_commit_key_file(&temporary_path, temporary_file, output, force, contents);

    if result.is_err() {
        // The temporary file contains key material, so clean it up on every
        // failed pre-commit path. A successful rename has already removed it.
        let _ = fs::remove_file(&temporary_path);
    }

    result
}

fn write_and_commit_key_file(
    temporary_path: &Path,
    mut temporary_file: File,
    output: &Path,
    force: bool,
    contents: &[u8],
) -> eyre::Result<()> {
    temporary_file
        .write_all(contents)
        .map_err(|err| eyre::eyre!("failed writing P2P key `{}`: {err}", output.display()))?;
    temporary_file
        .sync_all()
        .map_err(|err| eyre::eyre!("failed syncing P2P key `{}`: {err}", output.display()))?;
    drop(temporary_file);

    if force {
        fs::rename(temporary_path, output)
            .map_err(|err| eyre::eyre!("failed replacing P2P key `{}`: {err}", output.display()))?;
    } else {
        // `hard_link` atomically creates the destination without replacing one
        // that appeared after the check above.
        fs::hard_link(temporary_path, output).map_err(|err| {
            eyre::eyre!("failed installing P2P key `{}`: {err}", output.display())
        })?;
        fs::remove_file(temporary_path).map_err(|err| {
            eyre::eyre!(
                "failed removing temporary P2P key `{}`: {err}",
                temporary_path.display()
            )
        })?;
    }

    Ok(())
}

fn create_temporary_key_file(parent: &Path, output: &Path) -> eyre::Result<(PathBuf, File)> {
    for _ in 0..TEMP_FILE_ATTEMPTS {
        let temporary_path = parent.join(format!(
            ".p2p-key-{}-{:016x}.tmp",
            std::process::id(),
            rand::rng().random::<u64>()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }

        match options.open(&temporary_path) {
            Ok(file) => return Ok((temporary_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(eyre::eyre!(
                    "failed creating temporary P2P key beside `{}`: {err}",
                    output.display()
                ));
            }
        }
    }

    Err(eyre::eyre!(
        "failed creating a unique temporary P2P key beside `{}`",
        output.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::Read as _,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::write_key_file;

    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "tempo-generate-p2p-key-test-{}-{}",
                std::process::id(),
                NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn force_replaces_the_inode_held_by_existing_readers() {
        let directory = TestDirectory::new();
        let output = directory.path().join("p2p.key");
        let legacy_key = b"legacy-world-readable-key\n";
        fs::write(&output, legacy_key).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&output, fs::Permissions::from_mode(0o644)).unwrap();
        }

        let mut existing_reader = fs::File::open(&output).unwrap();

        write_key_file(&output, true, b"replacement-key\n").unwrap();

        let mut retained_contents = Vec::new();
        existing_reader.read_to_end(&mut retained_contents).unwrap();
        assert_eq!(retained_contents, legacy_key);
        assert_eq!(fs::read(&output).unwrap(), b"replacement-key\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(&output).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn force_rejects_a_symlink_destination() {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new();
        let target = directory.path().join("target.key");
        let output = directory.path().join("p2p.key");
        fs::write(&target, b"do-not-replace\n").unwrap();
        symlink(&target, &output).unwrap();

        let err = write_key_file(&output, true, b"replacement-key\n").unwrap_err();
        assert!(err.to_string().contains("non-regular"));
        assert_eq!(fs::read(&target).unwrap(), b"do-not-replace\n");
    }
}
