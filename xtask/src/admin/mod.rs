use clap::Subcommand;

mod check;
mod config;
mod encryption_key;
mod invariants;
mod leader;
pub(crate) mod secret_file;
mod snapshot;

pub(crate) use check::Check;
pub(crate) use encryption_key::EncryptionKey;
pub(crate) use leader::Leader;
pub(crate) use snapshot::{PortalSnapshot, read_portal_snapshot};

#[derive(Debug, clap::Parser)]
pub(crate) struct Admin {
    #[command(subcommand)]
    command: AdminCommand,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Audit finalized Portal state and live Zone nodes without changing state.
    Check(Check),
    /// Prepare or register a shared sequencer encryption key.
    EncryptionKey(EncryptionKey),
    /// Move finalized Zone leadership.
    Leader(Leader),
}

impl Admin {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        match self.command {
            AdminCommand::Check(command) => command.run().await,
            AdminCommand::EncryptionKey(command) => command.run().await,
            AdminCommand::Leader(command) => command.run().await,
        }
    }
}
