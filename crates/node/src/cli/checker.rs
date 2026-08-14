use std::{ffi::OsString, path::PathBuf, time::Duration};

use alloy_primitives::Address;
use clap::{Args, Parser, Subcommand};
use reth_ethereum::cli::Cli;
use zone_chainspec::ZoneChainSpecParser;
use zone_checker::{CheckerConfig, CheckerMode};

use super::{NodeAction, ZoneArgs, run_node};

/// Manage checker checkpoints.
#[derive(Debug, Parser)]
#[command(name = "checker", about = "Manage checker checkpoints")]
pub struct CheckerCommand {
    #[command(subcommand)]
    command: CheckerSubcommand,
}

#[derive(Debug, Subcommand)]
enum CheckerSubcommand {
    /// Build and atomically publish a checkpoint using a local Zone node.
    BuildCheckpoint(CheckerBuildCheckpointArgs),
    /// Inspect durable checker progress and alert state.
    Inspect(CheckerInspectArgs),
}

/// Checkpoint output and node arguments.
#[derive(Debug, Args)]
struct CheckerBuildCheckpointArgs {
    /// Destination for the checker database.
    #[arg(long = "checker.database-path", value_name = "PATH")]
    database_path: PathBuf,

    /// The `node` subcommand and its arguments after `--`, including `--chain`,
    /// `--datadir`, `--l1.rpc-url`, `--l1.portal-address`, and `--zone.id`.
    #[arg(last = true, required = true, value_name = "NODE_ARGS")]
    node_args: Vec<OsString>,
}

/// Read-only checker database inspection.
#[derive(Debug, Args)]
struct CheckerInspectArgs {
    /// Path to the checker's dedicated database.
    #[arg(long = "checker.database-path", value_name = "PATH")]
    database_path: PathBuf,

    /// Print machine-readable JSON.
    #[arg(long)]
    json: bool,
}

impl CheckerCommand {
    pub(super) fn run(self) -> eyre::Result<()> {
        match self.command {
            CheckerSubcommand::BuildCheckpoint(args) => {
                validate_node_args(&args.node_args)?;
                let cli = Cli::<ZoneChainSpecParser, ZoneArgs>::try_parse_from(
                    std::iter::once(OsString::from("zone-node")).chain(args.node_args),
                )?;
                run_node(
                    cli,
                    NodeAction::BuildCheckpoint {
                        database_path: args.database_path,
                    },
                )
            }
            CheckerSubcommand::Inspect(args) => {
                let snapshot = zone_checker::inspection::inspect_database(args.database_path)?;
                if args.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "recoveryZoneTip": {
                                "number": snapshot.recovery_zone_tip.number,
                                "hash": snapshot.recovery_zone_tip.hash,
                            },
                            "verifiedZoneTip": {
                                "number": snapshot.verified_zone_tip.number,
                                "hash": snapshot.verified_zone_tip.hash,
                            },
                            "importedTempoTip": {
                                "number": snapshot.imported_tempo_tip.number,
                                "hash": snapshot.imported_tempo_tip.hash,
                            },
                            "observedZoneTip": {
                                "number": snapshot.observed_zone_tip.number,
                                "hash": snapshot.observed_zone_tip.hash,
                            },
                            "recovering": snapshot.recovering,
                            "activeFinding": snapshot.active_finding,
                            "clearedFindings": snapshot.cleared_findings,
                            "hasCoverageGap": snapshot.has_coverage_gap,
                            "blockedReason": snapshot.blocked_reason,
                        }))?
                    );
                } else {
                    println!(
                        "Recovery Zone tip:     {}/{}",
                        snapshot.recovery_zone_tip.number, snapshot.recovery_zone_tip.hash
                    );
                    println!(
                        "Verified Zone tip:     {}/{}",
                        snapshot.verified_zone_tip.number, snapshot.verified_zone_tip.hash
                    );
                    println!(
                        "Imported Tempo tip:    {}/{}",
                        snapshot.imported_tempo_tip.number, snapshot.imported_tempo_tip.hash
                    );
                    println!(
                        "Observed Zone tip:     {}/{}",
                        snapshot.observed_zone_tip.number, snapshot.observed_zone_tip.hash
                    );
                    println!("Recovering:            {}", snapshot.recovering);
                    println!("Active finding:       {}", snapshot.active_finding);
                    println!("Cleared findings:     {}", snapshot.cleared_findings);
                    println!("Coverage gap:         {}", snapshot.has_coverage_gap);
                    println!(
                        "Blocked reason:       {}",
                        snapshot
                            .blocked_reason
                            .map_or_else(|| "none".into(), |reason| reason.to_string())
                    );
                }
                Ok(())
            }
        }
    }
}

fn validate_node_args(arguments: &[OsString]) -> eyre::Result<()> {
    eyre::ensure!(
        !arguments
            .iter()
            .any(|argument| argument.to_string_lossy().starts_with("--checker.")),
        "checker options must appear before `--`"
    );
    Ok(())
}

/// Checker options.
#[derive(Debug, Clone, Args)]
pub struct CheckerArgs {
    /// Checker mode: `off` (default) or `observe`.
    #[arg(long = "checker.mode", env = "CHECKER_MODE", default_value = "off")]
    pub mode: CheckerMode,

    /// Path to the checker's dedicated database.
    #[arg(
        long = "checker.database-path",
        env = "CHECKER_DATABASE_PATH",
        value_name = "PATH"
    )]
    pub database_path: Option<PathBuf>,

    /// Maximum seconds for one checker acquisition attempt.
    #[arg(
        long = "checker.acquisition-timeout-secs",
        env = "CHECKER_ACQUISITION_TIMEOUT_SECS",
        default_value_t = 30,
        value_name = "SECONDS"
    )]
    pub acquisition_timeout_secs: u64,
}

impl CheckerArgs {
    pub(super) fn config(
        &self,
        l1_rpc_url: &str,
        portal_address: Address,
        zone_id: u32,
    ) -> eyre::Result<Option<CheckerConfig>> {
        match self.mode {
            CheckerMode::Off => {
                eyre::ensure!(
                    self.database_path.is_none(),
                    "--checker.database-path requires --checker.mode observe"
                );
                Ok(None)
            }
            CheckerMode::Observe => {
                eyre::ensure!(
                    zone_id != 0,
                    "--zone.id must not be zero with --checker.mode observe"
                );
                eyre::ensure!(
                    self.acquisition_timeout_secs != 0,
                    "--checker.acquisition-timeout-secs must not be zero"
                );
                let database_path = self.database_path.clone().ok_or_else(|| {
                    eyre::eyre!("--checker.database-path is required with --checker.mode observe")
                })?;
                Ok(Some(CheckerConfig {
                    l1_rpc_url: l1_rpc_url.to_owned(),
                    portal_address,
                    zone_id,
                    database_path,
                    acquisition_timeout: Duration::from_secs(self.acquisition_timeout_secs),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    use alloy_primitives::Address;
    use clap::Parser as _;
    use zone_checker::CheckerMode;

    use super::{CheckerArgs, CheckerCommand, CheckerSubcommand, validate_node_args};

    const PORTAL: Address = Address::repeat_byte(0x22);

    #[derive(Debug, clap::Parser)]
    struct Parser {
        #[command(flatten)]
        checker: CheckerArgs,
    }

    fn parse(arguments: impl IntoIterator<Item = &'static str>) -> CheckerArgs {
        Parser::try_parse_from(arguments).unwrap().checker
    }

    #[test]
    fn defaults_to_off_without_a_runtime_config() {
        let args = parse(["tempo-zone"]);

        assert_eq!(args.mode, CheckerMode::Off);
        assert!(
            args.config("ws://localhost:8546", PORTAL, 0)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn observe_builds_runtime_config() {
        let args = parse([
            "tempo-zone",
            "--checker.mode",
            "observe",
            "--checker.database-path",
            "checker-test-db",
        ]);

        let config = args
            .config("ws://localhost:8546", PORTAL, 7)
            .unwrap()
            .unwrap();
        assert_eq!(config.l1_rpc_url, "ws://localhost:8546");
        assert_eq!(config.portal_address, PORTAL);
        assert_eq!(config.zone_id, 7);
        assert_eq!(config.database_path, PathBuf::from("checker-test-db"));
        assert_eq!(config.acquisition_timeout, Duration::from_secs(30));
    }

    #[test]
    fn observe_requires_a_database_path() {
        let args = parse(["tempo-zone", "--checker.mode", "observe"]);

        let error = args.config("ws://localhost:8546", PORTAL, 7).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("--checker.database-path is required")
        );
    }

    #[test]
    fn observe_requires_a_nonzero_zone_id() {
        let args = parse(["tempo-zone", "--checker.mode", "observe"]);

        let error = args.config("ws://localhost:8546", PORTAL, 0).unwrap_err();
        assert!(error.to_string().contains("--zone.id must not be zero"));
    }

    #[test]
    fn checker_only_options_require_observe_mode() {
        let args = parse(["tempo-zone", "--checker.database-path", "checker-test-db"]);
        let error = args.config("ws://localhost:8546", PORTAL, 7).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires --checker.mode observe")
        );
    }

    #[test]
    fn unknown_mode_is_rejected_by_clap() {
        assert!(Parser::try_parse_from(["tempo-zone", "--checker.mode", "enforce"]).is_err());
    }

    #[test]
    fn build_checkpoint_requires_checker_fields_and_trailing_node_arguments() {
        let valid = CheckerCommand::try_parse_from([
            "checker",
            "build-checkpoint",
            "--checker.database-path",
            "checkpoint",
            "--",
            "--chain",
            "dev",
            "--l1.rpc-url",
            "ws://localhost:8546",
            "--l1.portal-address",
            "0x0000000000000000000000000000000000000001",
            "--zone.id",
            "1",
        ])
        .unwrap();
        assert!(matches!(
            valid.command,
            CheckerSubcommand::BuildCheckpoint(_)
        ));

        assert_eq!(
            CheckerCommand::try_parse_from(["checker", "build-checkpoint", "--", "--chain", "dev"])
                .unwrap_err()
                .kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn build_checkpoint_rejects_checker_options_after_separator() {
        let arguments = [OsString::from("--checker.mode=observe")];
        assert_eq!(
            validate_node_args(&arguments).unwrap_err().to_string(),
            "checker options must appear before `--`"
        );
    }

    #[test]
    fn build_checkpoint_rejects_a_configured_creation_block() {
        let error = CheckerCommand::try_parse_from([
            "checker",
            "build-checkpoint",
            "--checker.portal-creation-block-hash",
            "0x1111111111111111111111111111111111111111111111111111111111111111",
        ])
        .unwrap_err();

        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn inspect_accepts_database_path_and_json_output() {
        let command = CheckerCommand::try_parse_from([
            "checker",
            "inspect",
            "--checker.database-path",
            "checker-test-db",
            "--json",
        ])
        .unwrap();

        let CheckerSubcommand::Inspect(args) = command.command else {
            panic!("expected inspect command");
        };
        assert_eq!(args.database_path, PathBuf::from("checker-test-db"));
        assert!(args.json);
    }
}
