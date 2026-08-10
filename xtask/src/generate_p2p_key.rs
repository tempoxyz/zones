use std::{fs::OpenOptions, io::Write as _, path::PathBuf};

use commonware_codec::Encode as _;
use commonware_cryptography::{Signer as _, ed25519::PrivateKey};
use commonware_math::algebra::Random as _;

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
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(self.force);
        if self.force {
            options.create(true);
        } else {
            options.create_new(true);
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }

        let mut file = options.open(&self.output).map_err(|err| {
            eyre::eyre!("failed writing P2P key `{}`: {err}", self.output.display())
        })?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|err| {
                    eyre::eyre!(
                        "failed restricting P2P key permissions `{}`: {err}",
                        self.output.display()
                    )
                })?;
        }

        writeln!(
            file,
            "{}",
            const_hex::encode_prefixed(key.encode().as_ref())
        )
        .map_err(|err| eyre::eyre!("failed writing P2P key `{}`: {err}", self.output.display()))?;

        println!("{}", const_hex::encode_prefixed(key.public_key().as_ref()));
        Ok(())
    }
}
