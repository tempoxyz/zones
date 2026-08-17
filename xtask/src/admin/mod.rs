use clap::Subcommand;

mod check;

pub(crate) use check::{Check, PortalSnapshot, read_portal_snapshot};

#[derive(Debug, clap::Parser)]
pub(crate) struct Admin {
    #[command(subcommand)]
    command: AdminCommand,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Audit finalized Portal state and live Zone nodes without changing state.
    Check(Check),
}

impl Admin {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        match self.command {
            AdminCommand::Check(command) => command.run().await,
        }
    }
}
