use crate::{
    catalog::{CasePlan, Catalog, ExecutionMode, RunPlan},
    deployment::DeploymentContext,
    report::{
        CaseResult, CaseStatus, CoverageReport, Journal, RunReport, load_txgen_summary, unix_ms,
        write_json_atomic,
    },
};
use alloy::{
    network::ReceiptResponse,
    primitives::{Address, B256},
    providers::{PendingTransactionBuilder, Provider, ProviderBuilder},
};
use eyre::{Context as _, Result, bail, ensure};
use serde::Deserialize;
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Instant,
};
use tempo_alloy::TempoNetwork;
use tokio::{process::Command, sync::Mutex, task::JoinSet};

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed installing SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RunOptions {
    pub(crate) txgen_bin: PathBuf,
    pub(crate) report_dir: Option<PathBuf>,
    pub(crate) failure_policy: String,
    pub(crate) step_timeout: String,
    pub(crate) sample_instances: usize,
    pub(crate) allow_governance: bool,
    pub(crate) fund_accounts: bool,
    pub(crate) validate_only: bool,
}

pub(crate) struct RunOutcome {
    pub(crate) report: RunReport,
    pub(crate) report_path: PathBuf,
}

impl RunOutcome {
    pub(crate) fn succeeded(&self) -> bool {
        !self.report.cancelled
            && self
                .report
                .cases
                .iter()
                .all(|result| result.status == CaseStatus::Passed)
            && self.report.cases.len() == self.report.plan.cases.len()
    }
}

pub(crate) async fn check_txgen(txgen_bin: &Path) -> Result<String> {
    let output = Command::new(txgen_bin)
        .args(["scenario", "validate", "--help"])
        .output()
        .await
        .wrap_err_with(|| {
            format!(
                "failed executing {} scenario validate --help",
                txgen_bin.display()
            )
        })?;
    ensure!(
        output.status.success(),
        "{} scenario validate --help exited with {}: {}",
        txgen_bin.display(),
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("scenario validator available")
        .trim()
        .to_owned())
}

pub(crate) async fn validate_plan(
    plan: &RunPlan,
    deployment: &DeploymentContext,
    txgen_bin: &Path,
) -> Result<()> {
    let env = deployment.command_env();
    let requirements = plan_requirements(plan);
    let missing = deployment.missing_requirements(&requirements);
    ensure!(
        missing.is_empty(),
        "missing workload environment values: {}",
        missing.join(", ")
    );

    for case in &plan.cases {
        ensure!(
            case.scenario.is_file(),
            "scenario for {} does not exist: {}",
            case.id,
            case.scenario.display()
        );
        let output = Command::new(txgen_bin)
            .args(["scenario", "validate", "--scenario"])
            .arg(&case.scenario)
            .envs(&env)
            .output()
            .await
            .wrap_err_with(|| format!("failed validating scenario {}", case.id))?;
        ensure!(
            output.status.success(),
            "scenario {} is invalid: {}{}",
            case.id,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct FundingScenario {
    chains: BTreeMap<String, FundingChain>,
}

#[derive(Debug, Deserialize)]
struct FundingChain {
    workload: PathBuf,
}

async fn fund_plan_accounts(
    plan: &RunPlan,
    deployment: &DeploymentContext,
    txgen_bin: &Path,
) -> Result<usize> {
    let workloads = l1_workloads(plan)?;
    let env = deployment.command_env();
    let mut addresses = BTreeSet::new();

    for workload in workloads {
        let output = Command::new(txgen_bin)
            .args(["addresses", "--spec"])
            .arg(&workload)
            .args(["--format", "json"])
            .envs(&env)
            .output()
            .await
            .wrap_err_with(|| {
                format!(
                    "failed deriving funding addresses from {}",
                    workload.display()
                )
            })?;
        ensure!(
            output.status.success(),
            "failed deriving funding addresses from {}: {}{}",
            workload.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let derived: Vec<Address> = serde_json::from_slice(&output.stdout).wrap_err_with(|| {
            format!("failed parsing txgen addresses for {}", workload.display())
        })?;
        addresses.extend(derived);
    }

    ensure!(
        !addresses.is_empty(),
        "selected scenarios do not define any L1 workload accounts to fund"
    );
    eprintln!(
        "funding {} L1 workload account(s) through tempo_fundAddress...",
        addresses.len()
    );
    let provider = ProviderBuilder::new_with_network::<TempoNetwork>()
        .connect(&deployment.l1_rpc_url)
        .await
        .wrap_err("failed connecting to the L1 faucet RPC")?;
    for (index, address) in addresses.iter().copied().enumerate() {
        let hashes: Vec<B256> = provider
            .raw_request("tempo_fundAddress".into(), (address,))
            .await
            .wrap_err_with(|| format!("tempo_fundAddress failed for {address}"))?;
        ensure!(
            !hashes.is_empty(),
            "tempo_fundAddress returned no transactions for {address}"
        );
        for hash in hashes {
            let receipt = PendingTransactionBuilder::new(provider.root().clone(), hash)
                .get_receipt()
                .await
                .wrap_err_with(|| format!("failed waiting for funding transaction {hash}"))?;
            ensure!(
                receipt.status(),
                "funding transaction {hash} reverted for {address}"
            );
        }
        let completed = index + 1;
        if completed % 10 == 0 || completed == addresses.len() {
            eprintln!(
                "funded {completed}/{} L1 workload account(s)",
                addresses.len()
            );
        }
    }
    Ok(addresses.len())
}

fn l1_workloads(plan: &RunPlan) -> Result<BTreeSet<PathBuf>> {
    let mut workloads = BTreeSet::new();
    for case in &plan.cases {
        let contents = std::fs::read_to_string(&case.scenario).wrap_err_with(|| {
            format!(
                "failed reading scenario {} for account funding",
                case.scenario.display()
            )
        })?;
        let scenario: FundingScenario = serde_yaml::from_str(&contents).wrap_err_with(|| {
            format!(
                "failed parsing scenario {} for account funding",
                case.scenario.display()
            )
        })?;
        let l1 = scenario.chains.get("l1").ok_or_else(|| {
            eyre::eyre!(
                "scenario {} has no `l1` chain to fund",
                case.scenario.display()
            )
        })?;
        let parent = case.scenario.parent().unwrap_or_else(|| Path::new("."));
        let workload = parent.join(&l1.workload).canonicalize().wrap_err_with(|| {
            format!(
                "failed resolving L1 workload {} from scenario {}",
                l1.workload.display(),
                case.scenario.display()
            )
        })?;
        workloads.insert(workload);
    }
    Ok(workloads)
}

pub(crate) async fn execute_plan(
    catalog: &Catalog,
    deployment: DeploymentContext,
    plan: RunPlan,
    options: RunOptions,
) -> Result<RunOutcome> {
    ensure!(!plan.cases.is_empty(), "run plan has no cases");
    ensure!(
        options.failure_policy == "continue" || options.failure_policy == "fail-fast",
        "failure policy must be continue or fail-fast"
    );
    if let Some(case) = plan.cases.iter().find(|case| case.dangerous)
        && !options.allow_governance
    {
        bail!(
            "case {} mutates governance; rerun with --allow-governance against a disposable deployment",
            case.id
        );
    }
    validate_mutation_groups(&plan)?;
    validate_plan(&plan, &deployment, &options.txgen_bin).await?;
    let funded_accounts = if options.fund_accounts && !options.validate_only {
        fund_plan_accounts(&plan, &deployment, &options.txgen_bin).await?
    } else {
        0
    };

    let started_at_unix_ms = unix_ms();
    let run_id = format!(
        "{}-{}-{:016x}",
        started_at_unix_ms,
        std::process::id(),
        plan.seed
    );
    let report_dir = options
        .report_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from("target/zones-load").join(&run_id));
    let report_path = report_dir.join("run.json");
    ensure!(
        !report_path.exists(),
        "refusing to overwrite existing report {}",
        report_path.display()
    );
    std::fs::create_dir_all(report_dir.join("cases"))
        .wrap_err_with(|| format!("failed creating report directory {}", report_dir.display()))?;

    let journal_path = report_dir.join("run.ndjson");
    let journal = Arc::new(Mutex::new(Journal::create(&journal_path, &run_id)?));
    journal.lock().await.record(
        "run_started",
        None,
        Some(json!({
            "profile": plan.profile,
            "seed": plan.seed,
            "execution": plan.execution,
            "case_count": plan.cases.len(),
        })),
    )?;

    if options.validate_only {
        journal.lock().await.record("run_validated", None, None)?;
    }

    let started = Instant::now();
    let (mut results, cancelled) = if options.validate_only {
        (Vec::new(), false)
    } else {
        match plan.execution {
            ExecutionMode::Sequential => {
                run_sequential(&plan, &deployment, &options, &report_dir, &run_id, &journal).await
            }
            ExecutionMode::Parallel => {
                run_parallel(&plan, &deployment, &options, &report_dir, &run_id, &journal).await
            }
        }
    };
    results.sort_by_key(|result| {
        plan.cases
            .iter()
            .position(|case| case.id == result.plan.id)
            .unwrap_or(usize::MAX)
    });

    let coverage = CoverageReport::build(catalog, &plan, &results);
    let report = RunReport {
        version: 1,
        run_id: run_id.clone(),
        profile: plan.profile.clone(),
        seed: plan.seed,
        started_at_unix_ms,
        elapsed_ms: started.elapsed().as_millis(),
        cancelled,
        txgen_bin: options.txgen_bin.display().to_string(),
        failure_policy: options.failure_policy.clone(),
        step_timeout: options.step_timeout.clone(),
        funded_accounts,
        deployment: deployment.sanitized(),
        plan,
        cases: results,
        coverage,
    };
    write_json_atomic(&report_path, &report)?;
    journal.lock().await.record(
        if cancelled {
            "run_cancelled"
        } else {
            "run_completed"
        },
        None,
        Some(json!({
            "report": report_path,
            "passed": report.cases.iter().filter(|case| case.status == CaseStatus::Passed).count(),
            "failed": report.cases.iter().filter(|case| case.status == CaseStatus::Failed).count(),
        })),
    )?;

    Ok(RunOutcome {
        report,
        report_path,
    })
}

async fn run_sequential(
    plan: &RunPlan,
    deployment: &DeploymentContext,
    options: &RunOptions,
    report_dir: &Path,
    run_id: &str,
    journal: &Arc<Mutex<Journal>>,
) -> (Vec<CaseResult>, bool) {
    let mut results = Vec::with_capacity(plan.cases.len());
    let mut cancelled = false;
    for case in &plan.cases {
        let execution = run_case(
            case.clone(),
            deployment.clone(),
            options.clone(),
            report_dir.to_path_buf(),
            run_id.to_owned(),
            Arc::clone(journal),
        );
        let result = tokio::select! {
            result = execution => result,
            _ = shutdown_signal() => {
                cancelled = true;
                break;
            }
        };
        let failed = result.status != CaseStatus::Passed;
        results.push(result);
        if failed && options.failure_policy == "fail-fast" {
            break;
        }
    }
    (results, cancelled)
}

async fn run_parallel(
    plan: &RunPlan,
    deployment: &DeploymentContext,
    options: &RunOptions,
    report_dir: &Path,
    run_id: &str,
    journal: &Arc<Mutex<Journal>>,
) -> (Vec<CaseResult>, bool) {
    let mut set = JoinSet::new();
    for case in &plan.cases {
        set.spawn(run_case(
            case.clone(),
            deployment.clone(),
            options.clone(),
            report_dir.to_path_buf(),
            run_id.to_owned(),
            Arc::clone(journal),
        ));
    }

    let mut results = Vec::with_capacity(plan.cases.len());
    let mut cancelled = false;
    while !set.is_empty() {
        tokio::select! {
            joined = set.join_next() => {
                match joined {
                    Some(Ok(result)) => {
                        let failed = result.status != CaseStatus::Passed;
                        results.push(result);
                        if failed && options.failure_policy == "fail-fast" {
                            set.abort_all();
                            cancelled = true;
                        }
                    }
                    Some(Err(error)) if !error.is_cancelled() => {
                        eprintln!("case task failed: {error}");
                        set.abort_all();
                        cancelled = true;
                    }
                    Some(Err(_)) => {}
                    None => break,
                }
            }
            _ = shutdown_signal() => {
                set.abort_all();
                cancelled = true;
            }
        }
    }
    (results, cancelled)
}

async fn run_case(
    plan: CasePlan,
    deployment: DeploymentContext,
    options: RunOptions,
    report_dir: PathBuf,
    run_id: String,
    journal: Arc<Mutex<Journal>>,
) -> CaseResult {
    let started = Instant::now();
    let txgen_report = report_dir.join("cases").join(format!("{}.json", plan.id));
    let mut command = Command::new(&options.txgen_bin);
    command
        .args(["scenario", "run", "--scenario"])
        .arg(&plan.scenario)
        .args(["--seed", &plan.seed.to_string()])
        .args(["--starts-per-second", &format_rate(plan.starts_per_second)])
        .args(["--tx-rate", &plan.transactions_per_second.to_string()])
        .args(["--max-in-flight", &plan.max_in_flight.to_string()])
        .args(["--max-rpc-in-flight", &plan.max_rpc_in_flight.to_string()])
        .args(["--failure-policy", &options.failure_policy])
        .args(["--step-timeout", &options.step_timeout])
        .args(["--sample-instances", &options.sample_instances.to_string()])
        .args(["--report"])
        .arg(&txgen_report)
        .args(["--metadata", &format!("zones-load-run-id={run_id}")])
        .args(["--metadata", &format!("zones-load-case={}", plan.id)])
        .envs(deployment.command_env())
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(count) = plan.count {
        command.args(["--count", &count.to_string()]);
    }
    if let Some(duration) = &plan.duration {
        command.args(["--duration", duration]);
    }

    let _ = journal.lock().await.record(
        "case_started",
        Some(&plan.id),
        Some(json!({
            "scenario": plan.scenario,
            "seed": plan.seed,
            "count": plan.count,
            "duration": plan.duration,
            "covers": plan.covers,
        })),
    );

    let (status, exit_code, txgen, error) = match command.spawn() {
        Ok(mut child) => match child.wait().await {
            Ok(exit) => {
                let summary = load_txgen_summary(&txgen_report);
                match summary {
                    Ok(summary)
                        if exit.success()
                            && summary.started > 0
                            && summary.completed == summary.started
                            && summary.failed == 0
                            && summary.timed_out == 0 =>
                    {
                        (CaseStatus::Passed, exit.code(), Some(summary), None)
                    }
                    Ok(summary) => (
                        CaseStatus::Failed,
                        exit.code(),
                        Some(summary),
                        Some(format!("txgen exited with {exit}")),
                    ),
                    Err(error) => (
                        CaseStatus::Failed,
                        exit.code(),
                        None,
                        Some(error.to_string()),
                    ),
                }
            }
            Err(error) => (
                CaseStatus::Failed,
                None,
                None,
                Some(format!("failed waiting for txgen: {error}")),
            ),
        },
        Err(error) => (
            CaseStatus::Failed,
            None,
            None,
            Some(format!("failed starting txgen: {error}")),
        ),
    };

    let result = CaseResult {
        plan,
        status,
        exit_code,
        elapsed_ms: started.elapsed().as_millis(),
        txgen_report,
        txgen,
        error,
    };
    let _ = journal.lock().await.record(
        "case_completed",
        Some(&result.plan.id),
        Some(json!({
            "status": result.status,
            "exit_code": result.exit_code,
            "elapsed_ms": result.elapsed_ms,
            "error": result.error,
        })),
    );
    result
}

fn plan_requirements(plan: &RunPlan) -> Vec<String> {
    plan.cases
        .iter()
        .flat_map(|case| case.requires.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn validate_mutation_groups(plan: &RunPlan) -> Result<()> {
    if plan.execution != ExecutionMode::Parallel {
        return Ok(());
    }
    let groups = plan
        .cases
        .iter()
        .filter_map(|case| case.mutation_group.as_ref())
        .collect::<Vec<_>>();
    ensure!(
        groups.is_empty(),
        "cases in mutation groups must use a sequential profile (found: {})",
        groups
            .into_iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(", ")
    );
    Ok(())
}

fn format_rate(rate: f64) -> String {
    let mut value = format!("{rate:.6}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.push('0');
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_unique_l1_workloads_for_account_funding() {
        let scenario = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../contrib/loadtest/scenarios/encrypted-deposit.yml");
        let plan = RunPlan {
            version: 1,
            profile: "test".to_owned(),
            execution: ExecutionMode::Sequential,
            seed: 1,
            requested_count: Some(1),
            duration: None,
            forever: false,
            starts_per_second: 0.0,
            transactions_per_second: 0,
            max_in_flight: 1,
            max_rpc_in_flight: 1,
            cases: vec![CasePlan {
                id: "encrypted-deposit".to_owned(),
                description: "test".to_owned(),
                scenario,
                seed: 1,
                count: Some(1),
                duration: None,
                starts_per_second: 0.0,
                transactions_per_second: 0,
                max_in_flight: 1,
                max_rpc_in_flight: 1,
                dangerous: false,
                mutation_group: None,
                covers: vec!["test".to_owned()],
                requires: Vec::new(),
            }],
        };

        let workloads = l1_workloads(&plan).unwrap();
        assert_eq!(workloads.len(), 1);
        assert!(
            workloads
                .iter()
                .next()
                .unwrap()
                .ends_with("scenarios/l1.yml")
        );
    }

    #[test]
    fn rates_are_cli_friendly() {
        assert_eq!(format_rate(0.0), "0.0");
        assert_eq!(format_rate(12.5), "12.5");
        assert_eq!(format_rate(0.333333333), "0.333333");
    }
}
