//! Stateful multi-chain load and codepath coverage runner for Tempo Zones.

mod catalog;
mod deployment;
mod report;
mod runner;

use crate::{
    catalog::{Catalog, PlanRequest, RunPlan},
    deployment::{DeploymentContext, DoctorCheck},
    report::{CaseStatus, CoverageReport, load_run_report, write_json_atomic},
    runner::{RunOptions, check_txgen, execute_plan, validate_plan},
};
use alloy::primitives::Address;
use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Context as _, Result, bail, ensure};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// ZonePortal contract address on Tempo L1.
    #[arg(long, global = true, env = "L1_PORTAL_ADDRESS")]
    portal: Option<Address>,

    /// Numeric Zone ID registered by the Portal.
    #[arg(long, global = true, env = "ZONES_LOAD_ZONE_ID")]
    zone_id: Option<u32>,

    /// TIP-20 token to use for deposits, withdrawals, and transfers.
    #[arg(long, global = true, env = "ZONES_LOAD_TOKEN")]
    token: Option<Address>,

    /// Tempo L1 HTTP RPC URL.
    #[arg(long, global = true, env = "L1_RPC_URL")]
    l1_rpc_url: Option<String>,

    /// Unrestricted Zone HTTP RPC used for queries and observations.
    #[arg(long, global = true, env = "ZONE_RPC_URL")]
    zone_rpc_url: Option<String>,

    /// Zone submission RPC; defaults to ZONE_REDACTED_RPC_URL, then the query RPC.
    #[arg(long, global = true, env = "ZONE_REDACTED_RPC_URL")]
    zone_submit_rpc_url: Option<String>,

    /// txgen Tempo executable.
    #[arg(
        long,
        global = true,
        env = "TXGEN_TEMPO_BIN",
        default_value = "txgen-tempo"
    )]
    txgen_bin: PathBuf,

    /// Optional custom workload catalog. The built-in catalog is used when omitted.
    #[arg(long, global = true)]
    catalog_file: Option<PathBuf>,

    /// Root used to resolve scenario paths from the catalog.
    #[arg(long, global = true, default_value = ".")]
    scenario_root: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Debug, Subcommand)]
enum Command {
    /// Check RPCs, deployment identity, fixtures, workload inputs, and scenario validity.
    Doctor(DoctorArgs),
    /// List workload profiles, cases, and stable coverage labels.
    Catalog(CatalogArgs),
    /// Resolve and print the exact deterministic execution plan without submitting transactions.
    Plan(PlanArgs),
    /// Validate and execute a workload profile.
    Run(RunArgs),
    /// Replay one case from a previous run with its original deterministic seed.
    Replay(ReplayArgs),
    /// Render codepath coverage from a run report.
    Coverage(CoverageArgs),
}

#[derive(Clone, Debug, Args)]
struct SelectionArgs {
    /// Built-in or custom profile name.
    #[arg(long, default_value = "branch-sweep")]
    profile: String,

    /// Select a particular case; repeat to select multiple cases.
    #[arg(long = "case")]
    cases: Vec<String>,

    /// Master deterministic seed.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Total journeys across the selected cases. Defaults to the profile setting.
    #[arg(long, conflicts_with = "duration")]
    count: Option<u64>,

    /// Run duration, such as 30s or 10m. Supported by parallel profiles or one case.
    #[arg(long, conflicts_with = "count")]
    duration: Option<String>,

    /// Keep starting journeys until interrupted. Supported by parallel profiles or one case.
    #[arg(long, conflicts_with_all = ["count", "duration"])]
    forever: bool,

    /// Aggregate journey starts per second; zero means unlimited.
    #[arg(long, default_value_t = 0.0)]
    journeys_per_second: f64,

    /// Aggregate transaction submissions per second on each chain; zero means unlimited.
    #[arg(long, default_value_t = 0)]
    transactions_per_second: u64,

    /// Aggregate active journey limit.
    #[arg(long, default_value_t = 20)]
    max_in_flight: usize,

    /// Aggregate in-flight transaction-submission RPC limit.
    #[arg(long, default_value_t = 100)]
    max_rpc_in_flight: usize,
}

#[derive(Clone, Debug, Args)]
struct DoctorArgs {
    /// Profile whose scenario files and environment requirements should be checked.
    #[arg(long, default_value = "smoke")]
    profile: String,

    /// Select a particular case; repeat to select multiple cases.
    #[arg(long = "case")]
    cases: Vec<String>,

    /// Do not invoke txgen's offline scenario validator.
    #[arg(long)]
    skip_scenario_validation: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Args)]
struct CatalogArgs {
    /// Limit output to cases in one profile.
    #[arg(long)]
    profile: Option<String>,

    /// Include catalogued cases that do not yet have executable scenarios.
    #[arg(long)]
    include_planned: bool,

    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Debug, Args)]
struct PlanArgs {
    #[command(flatten)]
    selection: SelectionArgs,

    /// Write the JSON plan to this path instead of stdout.
    #[arg(long)]
    output: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FailurePolicy {
    Continue,
    FailFast,
}

impl FailurePolicy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::FailFast => "fail-fast",
        }
    }
}

#[derive(Clone, Debug, Args)]
struct RunArgs {
    #[command(flatten)]
    selection: SelectionArgs,

    /// Directory for run.json, run.ndjson, and per-case txgen reports.
    #[arg(long)]
    report_dir: Option<PathBuf>,

    /// Continue independent cases after a failure, or stop promptly.
    #[arg(long, value_enum, default_value_t = FailurePolicy::Continue)]
    failure_policy: FailurePolicy,

    /// Default timeout for an individual scenario step.
    #[arg(long, default_value = "10m")]
    step_timeout: String,

    /// Number of sanitized txgen journey lifecycle samples to retain per case.
    #[arg(long, default_value_t = 10)]
    sample_instances: usize,

    /// Required before running a case that mutates Portal governance.
    #[arg(long)]
    allow_governance: bool,

    /// Fund every L1 workload signer through the devnet tempo_fundAddress RPC before running.
    #[arg(long)]
    fund_accounts: bool,

    /// Skip live RPC and deployment preflight checks.
    #[arg(long)]
    skip_doctor: bool,

    /// Print the resolved plan and exit without validation or submission.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Debug, Args)]
struct ReplayArgs {
    /// zones-load run.json from a previous execution.
    report: PathBuf,

    /// Case to replay. Defaults to the first failed case.
    #[arg(long = "case")]
    case: Option<String>,

    /// Override the original case journey count.
    #[arg(long)]
    count: Option<u64>,

    /// Directory for the replay report.
    #[arg(long)]
    report_dir: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = FailurePolicy::FailFast)]
    failure_policy: FailurePolicy,

    #[arg(long, default_value = "10m")]
    step_timeout: String,

    #[arg(long, default_value_t = 10)]
    sample_instances: usize,

    /// Fund every L1 workload signer through the devnet tempo_fundAddress RPC before replaying.
    #[arg(long)]
    fund_accounts: bool,
}

#[derive(Clone, Debug, Args)]
struct CoverageArgs {
    /// zones-load run.json.
    report: PathBuf,

    /// Include uncovered labels belonging only to not-yet-implemented cases.
    #[arg(long)]
    include_planned: bool,

    /// Exit nonzero when selected coverage is below this percentage.
    #[arg(long)]
    fail_under: Option<f64>,

    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    let cli = Cli::parse();
    let catalog = Catalog::load(cli.catalog_file.as_deref())?;
    match cli.command.clone() {
        Command::Catalog(args) => catalog_command(&catalog, args),
        Command::Plan(args) => plan_command(&catalog, &cli.scenario_root, args),
        Command::Doctor(args) => doctor_command(&cli, &catalog, args).await,
        Command::Run(args) => run_command(&cli, &catalog, args).await,
        Command::Replay(args) => replay_command(&cli, &catalog, args).await,
        Command::Coverage(args) => coverage_command(&catalog, args),
    }
}

fn catalog_command(catalog: &Catalog, args: CatalogArgs) -> Result<()> {
    let selected = match &args.profile {
        Some(profile) => Some(
            catalog
                .profiles
                .get(profile)
                .ok_or_else(|| eyre::eyre!("unknown profile {profile}"))?,
        ),
        None => None,
    };
    let ids = selected.map(|profile| profile.cases.iter().cloned().collect::<BTreeSet<_>>());
    let cases = catalog
        .cases
        .iter()
        .filter(|(id, case)| {
            ids.as_ref().is_none_or(|ids| ids.contains(*id))
                && (case.implemented || args.include_planned)
        })
        .collect::<Vec<_>>();

    if args.json {
        let profiles = args.profile.as_ref().map_or_else(
            || serde_json::to_value(&catalog.profiles),
            |name| serde_json::to_value([(name, selected.expect("profile resolved"))]),
        )?;
        let cases = cases
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": catalog.version,
                "profiles": profiles,
                "cases": cases,
            }))?
        );
        return Ok(());
    }

    if args.profile.is_none() {
        println!("Profiles");
        for (name, profile) in &catalog.profiles {
            println!(
                "  {name:<14} {:?}, default {} journeys — {}",
                profile.execution, profile.default_count, profile.description
            );
        }
        println!();
    }
    println!("Cases");
    for (id, case) in cases {
        let state = if case.implemented { "ready" } else { "planned" };
        println!("  {id:<30} [{state}] {}", case.description);
        for label in &case.covers {
            println!("    - {label}");
        }
    }
    Ok(())
}

fn plan_command(catalog: &Catalog, scenario_root: &Path, args: PlanArgs) -> Result<()> {
    let plan = build_plan(catalog, scenario_root, &args.selection)?;
    if let Some(output) = args.output {
        write_json_atomic(&output, &plan)?;
        println!("Plan written to {}", output.display());
    } else {
        println!("{}", serde_json::to_string_pretty(&plan)?);
    }
    Ok(())
}

async fn doctor_command(cli: &Cli, catalog: &Catalog, args: DoctorArgs) -> Result<()> {
    let selection = SelectionArgs {
        profile: args.profile,
        cases: args.cases,
        seed: 1,
        count: None,
        duration: None,
        forever: false,
        journeys_per_second: 0.0,
        transactions_per_second: 0,
        max_in_flight: 1,
        max_rpc_in_flight: 1,
    };
    let plan = build_plan(catalog, &cli.scenario_root, &selection)?;
    let deployment = load_deployment(cli).await?;
    let requirements = plan_requirements(&plan);
    let mut report = deployment.doctor(&requirements).await;

    let txgen_available = match check_txgen(&cli.txgen_bin).await {
        Ok(detail) => {
            report.checks.push(DoctorCheck {
                name: "txgen.capability".to_owned(),
                passed: true,
                detail,
            });
            true
        }
        Err(error) => {
            report.checks.push(DoctorCheck {
                name: "txgen.capability".to_owned(),
                passed: false,
                detail: format!("{error:#}"),
            });
            false
        }
    };
    if !args.skip_scenario_validation && report.missing_requirements.is_empty() && txgen_available {
        match validate_plan(&plan, &deployment, &cli.txgen_bin).await {
            Ok(()) => report.checks.push(DoctorCheck {
                name: "workload.scenarios".to_owned(),
                passed: true,
                detail: format!("{} scenario(s) valid", plan.cases.len()),
            }),
            Err(error) => report.checks.push(DoctorCheck {
                name: "workload.scenarios".to_owned(),
                passed: false,
                detail: format!("{error:#}"),
            }),
        }
    }
    report.healthy = report.checks.iter().all(|check| check.passed);

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let zone_chain_id = report
            .deployment
            .zone_chain_id
            .map_or_else(|| "unknown".to_owned(), |chain_id| chain_id.to_string());
        println!(
            "Deployment: Zone {} (chain {}) at {}; token {}",
            report.deployment.zone_id,
            zone_chain_id,
            report.deployment.portal,
            report.deployment.token
        );
        for check in &report.checks {
            println!(
                "  {} {:<30} {}",
                if check.passed { "PASS" } else { "FAIL" },
                check.name,
                check.detail
            );
        }
        if !report.missing_requirements.is_empty() {
            println!("Missing workload values:");
            for name in &report.missing_requirements {
                println!("  - {name}");
            }
        }
    }
    ensure!(report.healthy, "doctor found one or more failed checks");
    Ok(())
}

async fn run_command(cli: &Cli, catalog: &Catalog, args: RunArgs) -> Result<()> {
    let plan = build_plan(catalog, &cli.scenario_root, &args.selection)?;
    if args.dry_run {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }
    let deployment = load_deployment(cli).await?;
    if !args.skip_doctor {
        let doctor = deployment.doctor(&plan_requirements(&plan)).await;
        if !doctor.healthy {
            for check in doctor.checks.iter().filter(|check| !check.passed) {
                eprintln!("doctor failed: {}: {}", check.name, check.detail);
            }
            bail!("preflight checks failed; run `zones-load doctor` for details");
        }
    }

    let options = RunOptions {
        txgen_bin: cli.txgen_bin.clone(),
        report_dir: args.report_dir,
        failure_policy: args.failure_policy.as_str().to_owned(),
        step_timeout: args.step_timeout,
        sample_instances: args.sample_instances,
        allow_governance: args.allow_governance,
        fund_accounts: args.fund_accounts,
        validate_only: false,
    };
    let outcome = execute_plan(catalog, deployment, plan, options).await?;
    print_run_summary(&outcome.report, &outcome.report_path);
    ensure!(
        outcome.succeeded(),
        "one or more workload cases failed; report: {}",
        outcome.report_path.display()
    );
    Ok(())
}

async fn replay_command(cli: &Cli, catalog: &Catalog, args: ReplayArgs) -> Result<()> {
    let previous = load_run_report(&args.report)?;
    let selected = if let Some(id) = &args.case {
        previous
            .cases
            .iter()
            .find(|result| &result.plan.id == id)
            .ok_or_else(|| eyre::eyre!("case {id} is not present in {}", args.report.display()))?
    } else {
        previous
            .cases
            .iter()
            .find(|result| result.status != CaseStatus::Passed)
            .ok_or_else(|| eyre::eyre!("report has no failed case; pass --case to replay one"))?
    };
    let mut case = selected.plan.clone();
    if let Some(count) = args.count {
        ensure!(count > 0, "--count must be at least 1");
        case.count = Some(count);
        case.duration = None;
    }
    let replay_forever = previous.plan.forever && args.count.is_none();
    let plan = RunPlan {
        version: 1,
        profile: format!("replay:{}", case.id),
        execution: catalog::ExecutionMode::Sequential,
        seed: previous.seed,
        requested_count: if replay_forever { None } else { case.count },
        duration: case.duration.clone(),
        forever: replay_forever,
        starts_per_second: case.starts_per_second,
        transactions_per_second: case.transactions_per_second,
        max_in_flight: case.max_in_flight,
        max_rpc_in_flight: case.max_rpc_in_flight,
        cases: vec![case],
    };
    let deployment = load_deployment(cli).await.wrap_err(
        "failed resolving the current deployment for replay; pass --portal, --zone-id, --token, and RPC options explicitly (the report stores only sanitized endpoints)",
    )?;
    let options = RunOptions {
        txgen_bin: cli.txgen_bin.clone(),
        report_dir: args.report_dir,
        failure_policy: args.failure_policy.as_str().to_owned(),
        step_timeout: args.step_timeout,
        sample_instances: args.sample_instances,
        allow_governance: false,
        fund_accounts: args.fund_accounts,
        validate_only: false,
    };
    let outcome = execute_plan(catalog, deployment, plan, options).await?;
    print_run_summary(&outcome.report, &outcome.report_path);
    ensure!(
        outcome.succeeded(),
        "replayed case failed; report: {}",
        outcome.report_path.display()
    );
    Ok(())
}

fn coverage_command(catalog: &Catalog, args: CoverageArgs) -> Result<()> {
    let report = load_run_report(&args.report)?;
    let coverage = CoverageReport::build(catalog, &report.plan, &report.cases);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&coverage)?);
    } else {
        println!(
            "Selected coverage:    {}/{} ({:.1}%)",
            coverage.selected_covered, coverage.selected_total, coverage.selected_percent
        );
        println!(
            "Implemented catalog:  {}/{} ({:.1}%)",
            coverage.implemented_covered, coverage.implemented_total, coverage.implemented_percent
        );
        println!("Catalog labels:       {}", coverage.catalog_total);
        println!();
        for entry in coverage.entries.iter().filter(|entry| {
            entry.status != "covered" && (entry.implemented || args.include_planned)
        }) {
            println!(
                "  {:<10} {} ({})",
                entry.status,
                entry.label,
                entry.cases.join(", ")
            );
        }
    }
    if let Some(minimum) = args.fail_under {
        ensure!(
            minimum.is_finite() && (0.0..=100.0).contains(&minimum),
            "--fail-under must be between 0 and 100"
        );
        ensure!(
            coverage.selected_percent >= minimum,
            "selected coverage {:.1}% is below required {:.1}%",
            coverage.selected_percent,
            minimum
        );
    }
    Ok(())
}

fn build_plan(catalog: &Catalog, scenario_root: &Path, args: &SelectionArgs) -> Result<RunPlan> {
    catalog.plan(PlanRequest {
        profile: &args.profile,
        selected_cases: &args.cases,
        scenario_root,
        seed: args.seed,
        count: args.count,
        duration: args.duration.clone(),
        forever: args.forever,
        starts_per_second: args.journeys_per_second,
        transactions_per_second: args.transactions_per_second,
        max_in_flight: args.max_in_flight,
        max_rpc_in_flight: args.max_rpc_in_flight,
    })
}

async fn load_deployment(cli: &Cli) -> Result<DeploymentContext> {
    let deployment = DeploymentContext::new(
        cli.portal,
        cli.zone_id,
        cli.token,
        cli.l1_rpc_url.clone(),
        cli.zone_rpc_url.clone(),
        cli.zone_submit_rpc_url.clone(),
    )?;
    deployment.resolve().await
}

fn plan_requirements(plan: &RunPlan) -> Vec<String> {
    plan.cases
        .iter()
        .flat_map(|case| case.requires.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn print_run_summary(report: &report::RunReport, path: &Path) {
    let passed = report
        .cases
        .iter()
        .filter(|case| case.status == CaseStatus::Passed)
        .count();
    println!();
    println!("Run {}", report.run_id);
    println!("  Cases:     {passed}/{} passed", report.plan.cases.len());
    println!(
        "  Coverage:  {}/{} selected labels ({:.1}%)",
        report.coverage.selected_covered,
        report.coverage.selected_total,
        report.coverage.selected_percent
    );
    println!("  Report:    {}", path.display());
}
