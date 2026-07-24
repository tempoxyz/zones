use clap::Parser;
use eyre::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    fs,
    path::PathBuf,
};

/// Render a txgen benchmark report as a GitHub-compatible Markdown summary.
#[derive(Debug, Parser)]
pub(crate) struct BenchmarkResults {
    /// JSON report written by txgen.
    #[arg(long)]
    report: PathBuf,

    /// Rendered scenario YAML used to produce a scenario report.
    #[arg(long)]
    scenario: Option<PathBuf>,

    /// Destination for the rendered Markdown.
    #[arg(long)]
    output: PathBuf,
}

impl BenchmarkResults {
    pub(crate) fn run(self) -> Result<()> {
        let report = fs::read_to_string(&self.report)
            .wrap_err_with(|| format!("failed to read report {}", self.report.display()))?;
        let scenario = self
            .scenario
            .as_ref()
            .map(|path| {
                fs::read_to_string(path)
                    .wrap_err_with(|| format!("failed to read scenario {}", path.display()))
            })
            .transpose()?;
        let markdown = render_results(&report, scenario.as_deref())?;

        if let Some(parent) = self
            .output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!("failed to create output directory {}", parent.display())
            })?;
        }
        fs::write(&self.output, markdown)
            .wrap_err_with(|| format!("failed to write results to {}", self.output.display()))?;
        println!("Benchmark results written to {}", self.output.display());
        Ok(())
    }
}

fn render_results(report_json: &str, scenario_yaml: Option<&str>) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(report_json).wrap_err("failed to parse benchmark report JSON")?;
    if value.get("version").is_some() || value.get("scenario").is_some() {
        let report: ScenarioReport =
            serde_json::from_value(value).wrap_err("failed to parse txgen scenario report")?;
        let scenario_yaml = scenario_yaml
            .ok_or_else(|| eyre::eyre!("--scenario is required for a scenario report"))?;
        let scenario: ScenarioSpec = serde_yaml::from_str(scenario_yaml)
            .wrap_err("failed to parse rendered scenario YAML")?;
        render_scenario_results(&report, &scenario)
    } else {
        ensure!(
            scenario_yaml.is_none(),
            "--scenario can only be used with a scenario report"
        );
        let report: PhaseReport =
            serde_json::from_value(value).wrap_err("failed to parse txgen phase report")?;
        render_phase_results(&report)
    }
}

#[derive(Debug, Deserialize)]
struct ScenarioReport {
    version: u32,
    scenario: String,
    configuration: ScenarioConfiguration,
    elapsed_ms: u64,
    started: u64,
    completed: u64,
    failed: u64,
    timed_out: u64,
    completed_scenarios_per_second: f64,
    maximum_in_flight: usize,
    steps: Vec<StepReport>,
    total_scenario_latency: Latency,
    #[serde(default)]
    client_observed_e2e_latency: Option<Latency>,
    #[serde(default)]
    observed_critical_path_latency: Option<Latency>,
    #[serde(default)]
    causal_edges: Vec<CausalEdgeReport>,
    #[serde(default)]
    receipt_metrics: Vec<ReceiptMetricReport>,
    #[serde(default)]
    failures: Vec<FailureReport>,
    #[serde(default)]
    sampled_instances: Vec<serde_json::Value>,
}

impl ScenarioReport {
    fn client_observed_e2e_latency(&self) -> &Latency {
        self.client_observed_e2e_latency
            .as_ref()
            .unwrap_or(&self.total_scenario_latency)
    }
}

#[derive(Debug, Deserialize)]
struct ScenarioConfiguration {
    chains: Vec<ReportChain>,
    requested_instances: Option<u64>,
    starts_per_second: f64,
    maximum_in_flight: usize,
}

#[derive(Debug, Deserialize)]
struct ReportChain {
    name: String,
    chain_id: u64,
    #[serde(default)]
    observation_mode: Option<String>,
    #[serde(default)]
    observation_poll_interval_ms: Option<u64>,
    #[serde(default)]
    subscription_configured: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct StepReport {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    name: String,
    #[serde(default)]
    chain: Option<String>,
    kind: String,
    #[serde(default)]
    depends_on: Vec<String>,
    success: u64,
    failed: u64,
    latency: Latency,
    #[serde(default)]
    command_latency: Option<Latency>,
}

impl StepReport {
    fn command_latency(&self) -> &Latency {
        self.command_latency.as_ref().unwrap_or(&self.latency)
    }
}

#[derive(Debug, Deserialize)]
struct CausalEdgeReport {
    relation: String,
    source_step_id: String,
    destination_step_id: String,
    source_milestone: String,
    destination_milestone: String,
    observed_latency: Latency,
    chain_timestamp_delta: Latency,
    destination_observation_lag: Latency,
}

#[derive(Debug, Deserialize)]
struct FailureReport {
    step_index: usize,
    step_name: String,
    classification: String,
    count: u64,
}

#[derive(Debug, Deserialize)]
struct Latency {
    samples: usize,
    min_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

#[derive(Debug, Deserialize)]
struct ScenarioSpec {
    version: u32,
    chains: BTreeMap<String, ScenarioChain>,
    scenario: ScenarioDefinition,
}

#[derive(Debug, Deserialize)]
struct ScenarioChain {
    chain_id: u64,
}

#[derive(Debug, Deserialize)]
struct ScenarioDefinition {
    name: String,
    steps: Vec<ScenarioStep>,
}

#[derive(Debug, Deserialize)]
struct ScenarioStep {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    save: Option<String>,
    #[serde(default, rename = "timeout")]
    _timeout: Option<serde_yaml::Value>,
    #[serde(flatten)]
    operation: BTreeMap<String, serde_yaml::Value>,
}

struct ResolvedStep<'a> {
    report: &'a StepReport,
    chain: &'a str,
}

fn render_scenario_results(report: &ScenarioReport, scenario: &ScenarioSpec) -> Result<String> {
    validate_scenario_report(report, scenario)?;
    let steps = resolve_steps(report, scenario)?;
    let elapsed_secs = scenario_elapsed_secs(report)?;

    let submit_steps = steps
        .iter()
        .filter(|step| step.report.kind == "submit")
        .collect::<Vec<_>>();
    ensure!(
        !submit_steps.is_empty(),
        "scenario has no submit steps to report"
    );

    let submitted = submit_steps.iter().try_fold(0_u64, |total, step| {
        total
            .checked_add(step.report.success)
            .ok_or_else(|| eyre::eyre!("successful submit count overflow"))
    })?;
    let aggregate_tps = submitted as f64 / elapsed_secs;

    let mut output = String::new();
    writeln!(output, "# Zones benchmark results\n")?;
    writeln!(output, "Scenario: {}\n", code(&report.scenario))?;
    writeln!(output, "## Outcome\n")?;
    writeln!(
        output,
        "| Requested | Started | Completed | Failed | Timed out | Measured time |"
    )?;
    writeln!(output, "| ---: | ---: | ---: | ---: | ---: | ---: |")?;
    writeln!(
        output,
        "| {} | {} | {} | {} | {} | {} |\n",
        report
            .configuration
            .requested_instances
            .map_or_else(|| "duration-based".to_owned(), |value| value.to_string()),
        report.started,
        report.completed,
        report.failed,
        report.timed_out,
        format_seconds(report.elapsed_ms as f64 / 1_000.0),
    )?;
    writeln!(
        output,
        "Offered load: **{:.3} journey starts/s**. Maximum in flight: **{} observed / {} configured**.\n",
        report.configuration.starts_per_second,
        report.maximum_in_flight,
        report.configuration.maximum_in_flight,
    )?;

    write_observation_configuration(&mut output, &report.configuration.chains)?;

    writeln!(output, "## Throughput\n")?;
    writeln!(output, "| Scope | Count | Effective rate |")?;
    writeln!(output, "| --- | ---: | ---: |")?;
    writeln!(
        output,
        "| Complete journeys | {} | **{:.3} journeys/s** |",
        report.completed, report.completed_scenarios_per_second,
    )?;
    writeln!(
        output,
        "| All successful user submit steps | {submitted} | **{aggregate_tps:.3} TPS** |",
    )?;
    for chain in &report.configuration.chains {
        let mut has_submit_step = false;
        let mut successes = 0_u64;
        for step in &submit_steps {
            if step.chain != chain.name {
                continue;
            }
            has_submit_step = true;
            successes = successes
                .checked_add(step.report.success)
                .ok_or_else(|| eyre::eyre!("successful chain submit count overflow"))?;
        }
        if !has_submit_step {
            continue;
        }
        writeln!(
            output,
            "| {} successful user submit steps (chain ID {}) | {} | {:.3} TPS |",
            code(&chain.name),
            chain.chain_id,
            successes,
            successes as f64 / elapsed_secs,
        )?;
    }
    writeln!(output)?;

    writeln!(output, "### Transaction legs\n")?;
    writeln!(
        output,
        "| Step | Chain | Successful | Failed | Effective TPS | Submit-step p50 | Submit-step p95 |"
    )?;
    writeln!(output, "| --- | --- | ---: | ---: | ---: | ---: | ---: |")?;
    for step in &submit_steps {
        writeln!(
            output,
            "| {} | {} | {} | {} | {:.3} | {} | {} |",
            code(&step.report.name),
            code(step.chain),
            step.report.success,
            step.report.failed,
            step.report.success as f64 / elapsed_secs,
            format_millis(step.report.command_latency().p50_ms),
            format_millis(step.report.command_latency().p95_ms),
        )?;
    }
    writeln!(output)?;

    writeln!(output, "## Client-observed end-to-end journey latency\n")?;
    write_latency_header(&mut output)?;
    write_latency_row(&mut output, report.client_observed_e2e_latency())?;
    writeln!(output)?;

    if let Some(latency) = &report.observed_critical_path_latency {
        writeln!(output, "## Observed critical-path latency\n")?;
        write_latency_header(&mut output)?;
        write_latency_row(&mut output, latency)?;
        writeln!(output)?;
    }

    write_causal_edges(&mut output, &report.causal_edges)?;

    writeln!(output, "## Step command duration\n")?;
    writeln!(
        output,
        "| Step | Step ID | Depends on | Chain | Operation | Successful | Failed | Mean | P50 | P95 | P99 |"
    )?;
    writeln!(
        output,
        "| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for step in steps.iter().filter(|step| step.report.kind != "checkpoint") {
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            code(&step.report.name),
            code(step.report.id.as_deref().unwrap_or(&step.report.name)),
            if step.report.depends_on.is_empty() {
                "—".to_owned()
            } else {
                code(&step.report.depends_on.join(", "))
            },
            code(step.chain),
            code(&step.report.kind),
            step.report.success,
            step.report.failed,
            format_millis(step.report.command_latency().mean_ms),
            format_millis(step.report.command_latency().p50_ms),
            format_millis(step.report.command_latency().p95_ms),
            format_millis(step.report.command_latency().p99_ms),
        )?;
    }
    writeln!(output)?;

    if !report.sampled_instances.is_empty() {
        writeln!(
            output,
            "Sampled lifecycle traces retained in the JSON report: **{}**.\n",
            report.sampled_instances.len()
        )?;
    }

    write_receipt_metrics(&mut output, &report.receipt_metrics)?;

    if !report.failures.is_empty() {
        writeln!(output, "## Failure classifications\n")?;
        writeln!(output, "| Step | Classification | Count |")?;
        writeln!(output, "| --- | --- | ---: |")?;
        for failure in &report.failures {
            writeln!(
                output,
                "| {} (# {}) | {} | {} |",
                code(&failure.step_name),
                failure.step_index,
                code(&failure.classification),
                failure.count
            )?;
        }
        writeln!(output)?;
    }

    writeln!(
        output,
        "> Effective rates use the complete measured window, including ramp-up and drain. Aggregate user TPS sums submit steps across chains and is not a single-chain saturation result. Setup outside the measured scenario is excluded. Journey percentiles are computed from complete per-instance journeys; step and edge percentiles are never summed. Step command duration is client orchestration time, while causal edges separate observed dependency time, chain inclusion deltas, and destination observation lag."
    )?;

    Ok(output)
}

fn validate_scenario_report(report: &ScenarioReport, scenario: &ScenarioSpec) -> Result<()> {
    ensure!(
        matches!(report.version, 1 | 2),
        "unsupported txgen scenario report version {}",
        report.version
    );
    ensure!(
        scenario.version == 1,
        "unsupported rendered scenario version {}",
        scenario.version
    );
    ensure!(
        report.scenario == scenario.scenario.name,
        "report scenario {} does not match rendered scenario {}",
        report.scenario,
        scenario.scenario.name
    );
    ensure!(
        report.steps.len() == scenario.scenario.steps.len(),
        "report has {} steps but rendered scenario has {}",
        report.steps.len(),
        scenario.scenario.steps.len()
    );
    ensure!(
        !report.configuration.chains.is_empty(),
        "report has no configured chains"
    );
    ensure!(
        report.configuration.chains.len() == scenario.chains.len(),
        "report has {} chains but rendered scenario has {}",
        report.configuration.chains.len(),
        scenario.chains.len()
    );

    validate_nonnegative_finite(
        "completed scenario rate",
        report.completed_scenarios_per_second,
    )?;
    validate_nonnegative_finite(
        "configured start rate",
        report.configuration.starts_per_second,
    )?;
    validate_latency("total scenario latency", &report.total_scenario_latency)?;
    if report.version >= 2 {
        let client_observed = report.client_observed_e2e_latency.as_ref().ok_or_else(|| {
            eyre::eyre!("txgen scenario report version 2 has no client-observed E2E latency")
        })?;
        let critical_path = report
            .observed_critical_path_latency
            .as_ref()
            .ok_or_else(|| {
                eyre::eyre!("txgen scenario report version 2 has no observed critical-path latency")
            })?;
        validate_latency("client-observed E2E latency", client_observed)?;
        validate_latency("observed critical-path latency", critical_path)?;
    }
    let step_ids = report
        .steps
        .iter()
        .filter_map(|step| step.id.as_deref())
        .collect::<BTreeSet<_>>();
    if report.version >= 2 {
        ensure!(
            step_ids.len() == report.steps.len(),
            "txgen scenario report version 2 has missing or duplicate step IDs"
        );
    }
    for edge in &report.causal_edges {
        ensure!(
            step_ids.contains(edge.source_step_id.as_str()),
            "causal edge references unknown source step {}",
            edge.source_step_id
        );
        ensure!(
            step_ids.contains(edge.destination_step_id.as_str()),
            "causal edge references unknown destination step {}",
            edge.destination_step_id
        );
        validate_latency(
            &format!(
                "causal edge {} -> {} observed latency",
                edge.source_step_id, edge.destination_step_id
            ),
            &edge.observed_latency,
        )?;
        validate_signed_latency(
            &format!(
                "causal edge {} -> {} chain timestamp delta",
                edge.source_step_id, edge.destination_step_id
            ),
            &edge.chain_timestamp_delta,
        )?;
        validate_signed_latency(
            &format!(
                "causal edge {} -> {} destination observation lag",
                edge.source_step_id, edge.destination_step_id
            ),
            &edge.destination_observation_lag,
        )?;
    }

    let mut names = BTreeSet::new();
    for chain in &report.configuration.chains {
        ensure!(
            names.insert(chain.name.as_str()),
            "duplicate report chain {}",
            chain.name
        );
        if report.version >= 2 {
            ensure!(
                chain
                    .observation_mode
                    .as_deref()
                    .is_some_and(|mode| !mode.is_empty()),
                "report chain {} has no observation mode",
                chain.name
            );
            ensure!(
                chain
                    .observation_poll_interval_ms
                    .is_some_and(|milliseconds| milliseconds > 0),
                "report chain {} has no positive observation poll interval",
                chain.name
            );
            ensure!(
                chain.subscription_configured.is_some(),
                "report chain {} does not say whether subscription observation is configured",
                chain.name
            );
        }
        let rendered = scenario.chains.get(&chain.name).ok_or_else(|| {
            eyre::eyre!(
                "report chain {} is missing from rendered scenario",
                chain.name
            )
        })?;
        ensure!(
            chain.chain_id == rendered.chain_id,
            "report chain {} uses ID {} but rendered scenario uses {}",
            chain.name,
            chain.chain_id,
            rendered.chain_id
        );
    }
    Ok(())
}

fn resolve_steps<'a>(
    report: &'a ScenarioReport,
    scenario: &'a ScenarioSpec,
) -> Result<Vec<ResolvedStep<'a>>> {
    report
        .steps
        .iter()
        .zip(&scenario.scenario.steps)
        .enumerate()
        .map(|(index, (reported, rendered))| {
            ensure!(
                reported.index == index,
                "report step {} has index {}, expected {}",
                reported.name,
                reported.index,
                index
            );
            ensure!(
                rendered.operation.len() == 1,
                "rendered scenario step {} must contain exactly one operation",
                index + 1
            );
            let (kind, body) = rendered.operation.iter().next().expect("length checked");
            let expected_name = rendered
                .save
                .clone()
                .unwrap_or_else(|| format!("step_{}_{}", index + 1, kind));
            let expected_id = rendered
                .id
                .clone()
                .or_else(|| rendered.save.clone())
                .unwrap_or_else(|| format!("step_{}_{}", index + 1, kind));
            ensure!(
                reported.name == expected_name,
                "report step {} is named {} but rendered scenario names it {}",
                index,
                reported.name,
                expected_name
            );
            ensure!(
                reported.kind == *kind,
                "report step {} has kind {} but rendered scenario uses {}",
                reported.name,
                reported.kind,
                kind
            );
            if report.version >= 2 {
                ensure!(
                    reported.id.as_deref() == Some(expected_id.as_str()),
                    "report step {} has ID {:?} but rendered scenario uses {}",
                    reported.name,
                    reported.id,
                    expected_id
                );
                ensure!(
                    reported.depends_on == rendered.depends_on,
                    "report step {} has dependencies {:?} but rendered scenario uses {:?}",
                    reported.name,
                    reported.depends_on,
                    rendered.depends_on
                );
                ensure!(
                    reported.command_latency.is_some(),
                    "report step {} has no command latency",
                    reported.name
                );
            }
            let body = body.as_mapping().ok_or_else(|| {
                eyre::eyre!(
                    "rendered scenario step {} operation must be a mapping",
                    expected_name
                )
            })?;
            let chain = body
                .get(serde_yaml::Value::String("chain".to_owned()))
                .and_then(serde_yaml::Value::as_str)
                .ok_or_else(|| {
                    eyre::eyre!("rendered scenario step {} has no chain", expected_name)
                })?;
            ensure!(
                scenario.chains.contains_key(chain),
                "rendered scenario step {} references unknown chain {}",
                expected_name,
                chain
            );
            if let Some(reported_chain) = reported.chain.as_deref() {
                ensure!(
                    reported_chain == chain,
                    "report step {} uses chain {} but rendered scenario uses {}",
                    reported.name,
                    reported_chain,
                    chain
                );
            }
            validate_latency(
                &format!("step {} command latency", reported.name),
                reported.command_latency(),
            )?;
            Ok(ResolvedStep {
                report: reported,
                chain: reported.chain.as_deref().unwrap_or(chain),
            })
        })
        .collect()
}

fn scenario_elapsed_secs(report: &ScenarioReport) -> Result<f64> {
    let elapsed = if report.completed > 0 && report.completed_scenarios_per_second > 0.0 {
        report.completed as f64 / report.completed_scenarios_per_second
    } else {
        report.elapsed_ms as f64 / 1_000.0
    };
    ensure!(
        elapsed.is_finite() && elapsed > 0.0,
        "scenario measured time must be greater than zero"
    );
    Ok(elapsed)
}

fn write_observation_configuration(output: &mut String, chains: &[ReportChain]) -> Result<()> {
    if !chains.iter().any(|chain| {
        chain.observation_mode.is_some()
            || chain.observation_poll_interval_ms.is_some()
            || chain.subscription_configured.is_some()
    }) {
        return Ok(());
    }

    writeln!(output, "## Chain observation\n")?;
    writeln!(
        output,
        "| Chain | Mode | Poll fallback | Subscription configured |"
    )?;
    writeln!(output, "| --- | --- | ---: | --- |")?;
    for chain in chains {
        writeln!(
            output,
            "| {} | {} | {} | {} |",
            code(&chain.name),
            chain
                .observation_mode
                .as_deref()
                .map_or_else(|| "—".to_owned(), code),
            chain.observation_poll_interval_ms.map_or_else(
                || "—".to_owned(),
                |milliseconds| { format!("{milliseconds} ms") }
            ),
            chain
                .subscription_configured
                .map_or_else(|| "—".to_owned(), |configured| configured.to_string()),
        )?;
    }
    writeln!(output)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct PhaseReport {
    sent: u64,
    success: u64,
    failed: u64,
    elapsed_secs: f64,
    tps: f64,
    success_rate: f64,
    latency: Option<PhaseLatency>,
    #[serde(default)]
    receipt_metrics: Vec<ReceiptMetricReport>,
}

#[derive(Debug, Deserialize)]
struct ReceiptMetricReport {
    #[serde(default)]
    labels: BTreeMap<String, String>,
    gas_used: QuantityDistribution,
    effective_gas_price: QuantityDistribution,
    fee_paid: QuantityDistribution,
}

#[derive(Debug, Deserialize)]
struct QuantityDistribution {
    count: u64,
    min: Option<f64>,
    mean: Option<f64>,
    p50: Option<f64>,
    p95: Option<f64>,
    p99: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct PhaseLatency {
    min_ms: f64,
    max_ms: f64,
    mean_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
}

fn render_phase_results(report: &PhaseReport) -> Result<String> {
    ensure!(
        report.elapsed_secs.is_finite() && report.elapsed_secs > 0.0,
        "phase measured time must be greater than zero"
    );
    validate_nonnegative_finite("phase TPS", report.tps)?;
    validate_nonnegative_finite("phase acceptance rate", report.success_rate)?;
    ensure!(
        report.success <= report.sent,
        "successful count exceeds sent count"
    );
    ensure!(
        report.failed <= report.sent,
        "failed count exceeds sent count"
    );
    let mut output = String::new();
    writeln!(output, "# Zones benchmark results\n")?;
    writeln!(output, "## Outcome\n")?;
    writeln!(
        output,
        "| Sent | RPC accepted | Failed | Acceptance rate | Measured time |"
    )?;
    writeln!(output, "| ---: | ---: | ---: | ---: | ---: |")?;
    writeln!(
        output,
        "| {} | {} | {} | {:.3}% | {} |\n",
        report.sent,
        report.success,
        report.failed,
        report.success_rate,
        format_seconds(report.elapsed_secs)
    )?;

    writeln!(output, "## Throughput\n")?;
    writeln!(output, "| Scope | Effective rate |")?;
    writeln!(output, "| --- | ---: |")?;
    writeln!(
        output,
        "| Attempted transactions | **{:.3} TPS** |",
        report.tps
    )?;
    writeln!(
        output,
        "| RPC-accepted transactions | **{:.3} TPS** |\n",
        report.success as f64 / report.elapsed_secs
    )?;

    if let Some(latency) = &report.latency {
        validate_phase_latency(latency)?;
        writeln!(output, "## RPC response latency\n")?;
        writeln!(output, "| Samples | Min | Mean | P50 | P95 | P99 | Max |")?;
        writeln!(output, "| ---: | ---: | ---: | ---: | ---: | ---: | ---: |")?;
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} |\n",
            report.success,
            format_millis(latency.min_ms),
            format_millis(latency.mean_ms),
            format_millis(latency.p50_ms),
            format_millis(latency.p95_ms),
            format_millis(latency.p99_ms),
            format_millis(latency.max_ms)
        )?;
    }

    write_receipt_metrics(&mut output, &report.receipt_metrics)?;

    writeln!(
        output,
        "> Rates use the complete measured window, including ramp-up and drain. Latency ends when the RPC returns; setup transactions are excluded from the measured phase report. RPC-accepted and failed counts can overlap when an accepted transaction later reverts or its receipt wait fails."
    )?;
    Ok(output)
}

fn validate_latency(name: &str, latency: &Latency) -> Result<()> {
    for (field, value) in [
        ("min", latency.min_ms),
        ("max", latency.max_ms),
        ("mean", latency.mean_ms),
        ("p50", latency.p50_ms),
        ("p95", latency.p95_ms),
        ("p99", latency.p99_ms),
    ] {
        validate_nonnegative_finite(&format!("{name} {field}"), value)?;
    }
    if latency.samples > 0 {
        ensure!(
            latency.min_ms <= latency.max_ms,
            "{name} minimum exceeds maximum"
        );
    }
    Ok(())
}

fn validate_signed_latency(name: &str, latency: &Latency) -> Result<()> {
    for (field, value) in [
        ("min", latency.min_ms),
        ("max", latency.max_ms),
        ("mean", latency.mean_ms),
        ("p50", latency.p50_ms),
        ("p95", latency.p95_ms),
        ("p99", latency.p99_ms),
    ] {
        ensure!(value.is_finite(), "{name} {field} must be finite");
    }
    if latency.samples > 0 {
        ensure!(
            latency.min_ms <= latency.max_ms,
            "{name} minimum exceeds maximum"
        );
    }
    Ok(())
}

fn validate_phase_latency(latency: &PhaseLatency) -> Result<()> {
    for (field, value) in [
        ("phase latency min", latency.min_ms),
        ("phase latency max", latency.max_ms),
        ("phase latency mean", latency.mean_ms),
        ("phase latency p50", latency.p50_ms),
        ("phase latency p95", latency.p95_ms),
        ("phase latency p99", latency.p99_ms),
    ] {
        validate_nonnegative_finite(field, value)?;
    }
    ensure!(
        latency.min_ms <= latency.max_ms,
        "phase latency minimum exceeds maximum"
    );
    Ok(())
}

fn validate_nonnegative_finite(name: &str, value: f64) -> Result<()> {
    ensure!(
        value.is_finite() && value >= 0.0,
        "{name} must be finite and nonnegative"
    );
    Ok(())
}

fn write_latency_header(output: &mut String) -> Result<()> {
    writeln!(output, "| Samples | Min | Mean | P50 | P95 | P99 | Max |")?;
    writeln!(output, "| ---: | ---: | ---: | ---: | ---: | ---: | ---: |")?;
    Ok(())
}

fn write_latency_row(output: &mut String, latency: &Latency) -> Result<()> {
    writeln!(
        output,
        "| {} | {} | {} | {} | {} | {} | {} |",
        latency.samples,
        format_millis(latency.min_ms),
        format_millis(latency.mean_ms),
        format_millis(latency.p50_ms),
        format_millis(latency.p95_ms),
        format_millis(latency.p99_ms),
        format_millis(latency.max_ms),
    )?;
    Ok(())
}

fn write_causal_edges(output: &mut String, edges: &[CausalEdgeReport]) -> Result<()> {
    if edges.is_empty() {
        return Ok(());
    }

    writeln!(output, "## Causal-edge timing\n")?;
    writeln!(
        output,
        "| Dependency edge | Relation | Milestones | Timing | Samples | Mean | P50 | P95 | P99 |"
    )?;
    writeln!(
        output,
        "| --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for edge in edges {
        let dependency = code(&format!(
            "{} -> {}",
            edge.source_step_id, edge.destination_step_id
        ));
        let milestones = code(&format!(
            "{} -> {}",
            edge.source_milestone, edge.destination_milestone
        ));
        for (timing, latency) in [
            ("observed causal-edge latency", &edge.observed_latency),
            (
                "chain inclusion latency (timestamp delta)",
                &edge.chain_timestamp_delta,
            ),
            (
                "destination observation lag",
                &edge.destination_observation_lag,
            ),
        ] {
            writeln!(
                output,
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                dependency,
                code(&edge.relation),
                milestones,
                timing,
                latency.samples,
                format_millis(latency.mean_ms),
                format_millis(latency.p50_ms),
                format_millis(latency.p95_ms),
                format_millis(latency.p99_ms),
            )?;
        }
    }
    writeln!(output)?;
    Ok(())
}

fn write_receipt_metrics(output: &mut String, reports: &[ReceiptMetricReport]) -> Result<()> {
    if reports.is_empty() {
        return Ok(());
    }

    writeln!(output, "## Receipt gas metrics\n")?;
    writeln!(
        output,
        "| Input / labels | Metric | Count | Min | Mean | P50 | P95 | P99 |"
    )?;
    writeln!(
        output,
        "| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;

    for report in reports {
        let labels = format_receipt_labels(&report.labels);
        for (name, unit, distribution) in [
            ("gas_used", "gas", &report.gas_used),
            ("effective_gas_price", "wei", &report.effective_gas_price),
            ("fee_paid", "wei", &report.fee_paid),
        ] {
            validate_quantity_distribution(name, distribution)?;
            writeln!(
                output,
                "| {} | {} | {} | {} | {} | {} | {} | {} |",
                labels,
                code(name),
                distribution.count,
                format_quantity(distribution.min, unit),
                format_quantity(distribution.mean, unit),
                format_quantity(distribution.p50, unit),
                format_quantity(distribution.p95, unit),
                format_quantity(distribution.p99, unit),
            )?;
        }
    }
    writeln!(output)?;
    Ok(())
}

fn validate_quantity_distribution(name: &str, distribution: &QuantityDistribution) -> Result<()> {
    for (field, value) in [
        ("min", distribution.min),
        ("mean", distribution.mean),
        ("p50", distribution.p50),
        ("p95", distribution.p95),
        ("p99", distribution.p99),
    ] {
        if let Some(value) = value {
            validate_nonnegative_finite(&format!("receipt {name} {field}"), value)?;
        }
    }
    Ok(())
}

fn format_receipt_labels(labels: &BTreeMap<String, String>) -> String {
    if labels.is_empty() {
        return "—".to_owned();
    }

    code(
        &labels
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

fn format_quantity(value: Option<f64>, unit: &str) -> String {
    value.map_or_else(|| "—".to_owned(), |value| format!("{value:.3} {unit}"))
}

fn format_seconds(value: f64) -> String {
    format!("{value:.3} s")
}

fn format_millis(value: f64) -> String {
    if value.abs() >= 1_000.0 {
        format!("{:.3} s", value / 1_000.0)
    } else {
        format!("{value:.3} ms")
    }
}

fn code(value: &str) -> String {
    let escaped = value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "&#124;")
        .replace('`', "&#96;")
        .replace(['\n', '\r'], " ");
    format!("`{escaped}`")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCENARIO: &str = r#"
version: 1
chains:
  l1:
    chain_id: 1337
  zone:
    chain_id: 421700001
scenario:
  name: sample-roundtrip
  steps:
    - submit:
        chain: l1
      save: deposit
    - wait_log:
        chain: zone
      save: deposit_processed
    - submit:
        chain: zone
      save: activity
    - submit:
        chain: zone
      save: withdrawal
"#;

    const REPORT: &str = r#"
{
  "version": 1,
  "scenario": "sample-roundtrip",
  "configuration": {
    "chains": [
      {"name": "l1", "chain_id": 1337},
      {"name": "zone", "chain_id": 421700001}
    ],
    "requested_instances": 100,
    "starts_per_second": 10.0,
    "maximum_in_flight": 100
  },
  "elapsed_ms": 20000,
  "started": 100,
  "completed": 100,
  "failed": 0,
  "timed_out": 0,
  "completed_scenarios_per_second": 5.0,
  "maximum_in_flight": 41,
  "steps": [
    {"index": 0, "name": "deposit", "kind": "submit", "success": 100, "failed": 0, "latency": {"samples": 100, "min_ms": 0.1, "max_ms": 0.9, "mean_ms": 0.3, "p50_ms": 0.2, "p95_ms": 0.5, "p99_ms": 0.9}},
    {"index": 1, "name": "deposit_processed", "kind": "wait_log", "success": 100, "failed": 0, "latency": {"samples": 100, "min_ms": 400.0, "max_ms": 1200.0, "mean_ms": 600.0, "p50_ms": 500.0, "p95_ms": 1000.0, "p99_ms": 1200.0}},
    {"index": 2, "name": "activity", "kind": "submit", "success": 100, "failed": 0, "latency": {"samples": 100, "min_ms": 0.1, "max_ms": 0.8, "mean_ms": 0.3, "p50_ms": 0.2, "p95_ms": 0.5, "p99_ms": 0.8}},
    {"index": 3, "name": "withdrawal", "kind": "submit", "success": 99, "failed": 1, "latency": {"samples": 100, "min_ms": 0.1, "max_ms": 0.8, "mean_ms": 0.3, "p50_ms": 0.2, "p95_ms": 0.5, "p99_ms": 0.8}}
  ],
  "total_scenario_latency": {"samples": 100, "min_ms": 3000.0, "max_ms": 5000.0, "mean_ms": 4000.0, "p50_ms": 4100.0, "p95_ms": 4900.0, "p99_ms": 5000.0}
}
"#;

    const DAG_SCENARIO: &str = r#"
version: 1
chains:
  l1:
    chain_id: 1337
  zone:
    chain_id: 421700001
scenario:
  name: sample-roundtrip
  execution: dag
  steps:
    - id: deposit
      submit:
        chain: l1
      save: deposit
    - id: deposit_processed
      depends_on: [deposit]
      wait_log:
        chain: zone
      save: deposit_processed
    - id: activity
      depends_on: [deposit_processed]
      submit:
        chain: zone
      save: activity
    - id: withdrawal
      depends_on: [activity]
      submit:
        chain: zone
      save: withdrawal
"#;

    fn report_v2() -> serde_json::Value {
        let mut report: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        report["version"] = 2.into();
        report["configuration"]["chains"][0]["observation_mode"] = "auto".into();
        report["configuration"]["chains"][0]["observation_poll_interval_ms"] = 50.into();
        report["configuration"]["chains"][0]["subscription_configured"] = true.into();
        report["configuration"]["chains"][1]["observation_mode"] = "poll".into();
        report["configuration"]["chains"][1]["observation_poll_interval_ms"] = 50.into();
        report["configuration"]["chains"][1]["subscription_configured"] = false.into();

        let definitions = [
            ("deposit", "l1", Vec::<&str>::new()),
            ("deposit_processed", "zone", vec!["deposit"]),
            ("activity", "zone", vec!["deposit_processed"]),
            ("withdrawal", "zone", vec!["activity"]),
        ];
        for (step, (id, chain, dependencies)) in report["steps"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .zip(definitions)
        {
            step["id"] = id.into();
            step["chain"] = chain.into();
            step["depends_on"] = serde_json::json!(dependencies);
            step["command_latency"] = step["latency"].clone();
        }
        report["steps"][0]["command_latency"]["mean_ms"] = 13.0.into();
        report["steps"][0]["command_latency"]["p50_ms"] = 12.0.into();
        report["steps"][0]["command_latency"]["p95_ms"] = 18.0.into();
        report["steps"][0]["command_latency"]["p99_ms"] = 20.0.into();
        report["steps"][0]["command_latency"]["max_ms"] = 20.0.into();

        report["client_observed_e2e_latency"] = serde_json::json!({
            "samples": 100,
            "min_ms": 3200.0,
            "max_ms": 5100.0,
            "mean_ms": 4200.0,
            "p50_ms": 4250.0,
            "p95_ms": 4950.0,
            "p99_ms": 5100.0
        });
        report["observed_critical_path_latency"] = serde_json::json!({
            "samples": 100,
            "min_ms": 2500.0,
            "max_ms": 3900.0,
            "mean_ms": 3100.0,
            "p50_ms": 3100.0,
            "p95_ms": 3700.0,
            "p99_ms": 3900.0
        });
        report["causal_edges"] = serde_json::json!([{
            "relation": "dependency",
            "source_step_id": "deposit",
            "destination_step_id": "deposit_processed",
            "source_milestone": "receipt",
            "destination_milestone": "log",
            "observed_latency": {
                "samples": 100,
                "min_ms": 700.0,
                "max_ms": 1200.0,
                "mean_ms": 860.0,
                "p50_ms": 850.0,
                "p95_ms": 1100.0,
                "p99_ms": 1200.0
            },
            "chain_timestamp_delta": {
                "samples": 100,
                "min_ms": -1500.0,
                "max_ms": 1000.0,
                "mean_ms": 450.0,
                "p50_ms": 500.0,
                "p95_ms": 900.0,
                "p99_ms": 1000.0
            },
            "destination_observation_lag": {
                "samples": 100,
                "min_ms": 1.0,
                "max_ms": 52.0,
                "mean_ms": 26.0,
                "p50_ms": 25.0,
                "p95_ms": 48.0,
                "p99_ms": 52.0
            }
        }]);
        report["sampled_instances"] = serde_json::json!([
            {"instance": 0, "outcome": "completed"},
            {"instance": 1, "outcome": "completed"}
        ]);
        report
    }

    #[test]
    fn renders_generalized_scenario_throughput_and_latency() {
        let output = render_results(REPORT, Some(SCENARIO)).unwrap();

        assert!(output.contains("**5.000 journeys/s**"));
        assert!(output.contains("All successful user submit steps | 299 | **14.950 TPS**"));
        assert!(
            output.contains("`l1` successful user submit steps (chain ID 1337) | 100 | 5.000 TPS")
        );
        assert!(output.contains(
            "`zone` successful user submit steps (chain ID 421700001) | 199 | 9.950 TPS"
        ));
        assert!(output.contains("`withdrawal` | `zone` | 99 | 1 | 4.950"));
        assert!(
            output.contains("`deposit_processed` | `deposit_processed` | — | `zone` | `wait_log`")
        );
        assert!(output.contains("4.100 s"));
        assert!(!output.contains("Receipt gas metrics"));
    }

    #[test]
    fn renders_v2_dag_and_separates_causal_timing_from_command_duration() {
        let report = report_v2();
        let output =
            render_results(&serde_json::to_string(&report).unwrap(), Some(DAG_SCENARIO)).unwrap();

        assert!(output.contains("## Chain observation"));
        assert!(output.contains("| `l1` | `auto` | 50 ms | true |"));
        assert!(output.contains("| `zone` | `poll` | 50 ms | false |"));
        assert!(output.contains("## Client-observed end-to-end journey latency"));
        assert!(output.contains("4.250 s"));
        assert!(output.contains("## Observed critical-path latency"));
        assert!(output.contains("3.100 s"));
        assert!(output.contains("## Causal-edge timing"));
        assert!(output.contains("`deposit -&gt; deposit_processed`"));
        assert!(output.contains("observed causal-edge latency | 100 | 860.000 ms | 850.000 ms"));
        assert!(
            output.contains(
                "chain inclusion latency (timestamp delta) | 100 | 450.000 ms | 500.000 ms"
            )
        );
        assert!(
            output
                .contains("destination observation lag | 100 | 26.000 ms | 25.000 ms | 48.000 ms")
        );
        assert!(output.contains("## Step command duration"));
        assert!(output.contains(
            "`deposit` | `deposit` | — | `l1` | `submit` | 100 | 0 | 13.000 ms | 12.000 ms"
        ));
        assert!(output.contains("Sampled lifecycle traces retained in the JSON report: **2**."));
        assert!(output.contains("step and edge percentiles are never summed"));
    }

    #[test]
    fn validates_v2_dag_step_dependencies() {
        let report = report_v2();
        let scenario =
            DAG_SCENARIO.replace("depends_on: [deposit_processed]", "depends_on: [deposit]");
        let error =
            render_results(&serde_json::to_string(&report).unwrap(), Some(&scenario)).unwrap_err();

        assert!(error.to_string().contains(
            "report step activity has dependencies [\"deposit_processed\"] but rendered scenario uses [\"deposit\"]"
        ));
    }

    #[test]
    fn accepts_flattened_fragment_steps_and_runtime_provenance() {
        let scenario = SCENARIO.replacen(
            "      save: deposit\n",
            "      save: deposit_to_zone.submission\n",
            1,
        );
        let mut report: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        report["steps"][0]["name"] = "deposit_to_zone.submission".into();
        report["steps"][0]["provenance"] = serde_json::json!({
            "source_file": "scenario-fragments.yml",
            "fragment": "deposit-and-wait-zone",
            "instance_alias": "deposit_to_zone",
            "local_step_name": "submission",
            "local_step_index": 0
        });

        let output =
            render_results(&serde_json::to_string(&report).unwrap(), Some(&scenario)).unwrap();

        assert!(output.contains("`deposit_to_zone.submission` | `l1`"));
    }

    #[test]
    fn renders_receipt_metrics_for_multiple_labeled_scenario_inputs() {
        let mut report: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        report["receipt_metrics"] = serde_json::json!([
            {
                "labels": {
                    "step": "activity",
                    "input": "erc20|transfer",
                    "run_id": "run`1"
                },
                "gas_used": {
                    "count": 100,
                    "min": 21000.0,
                    "mean": 22000.0,
                    "p50": 21500.0,
                    "p95": 24000.0,
                    "p99": 25000.0
                },
                "effective_gas_price": {
                    "count": 100,
                    "min": 2.0,
                    "mean": 3.0,
                    "p50": 3.0,
                    "p95": 4.0,
                    "p99": 5.0
                },
                "fee_paid": {
                    "count": 100,
                    "min": 42000.0,
                    "mean": 66000.0,
                    "p50": 64500.0,
                    "p95": 96000.0,
                    "p99": 125000.0
                }
            },
            {
                "labels": {"input": "withdrawal", "step": "withdrawal"},
                "gas_used": {
                    "count": 1,
                    "min": 99000.0,
                    "mean": 99000.0,
                    "p50": 99000.0,
                    "p95": 99000.0,
                    "p99": 99000.0
                },
                "effective_gas_price": {
                    "count": 0,
                    "min": null,
                    "mean": null,
                    "p50": null,
                    "p95": null,
                    "p99": null
                },
                "fee_paid": {
                    "count": 0,
                    "min": null,
                    "mean": null,
                    "p50": null,
                    "p95": null,
                    "p99": null
                }
            }
        ]);

        let output =
            render_results(&serde_json::to_string(&report).unwrap(), Some(SCENARIO)).unwrap();

        assert!(output.contains("## Receipt gas metrics"));
        assert!(output.contains(
            "`input=erc20&#124;transfer, run_id=run&#96;1, step=activity` | `gas_used` | 100 | 21000.000 gas | 22000.000 gas | 21500.000 gas | 24000.000 gas | 25000.000 gas"
        ));
        assert!(output.contains(
            "`input=erc20&#124;transfer, run_id=run&#96;1, step=activity` | `fee_paid` | 100 | 42000.000 wei | 66000.000 wei | 64500.000 wei | 96000.000 wei | 125000.000 wei"
        ));
        assert!(output.contains(
            "`input=withdrawal, step=withdrawal` | `effective_gas_price` | 0 | — | — | — | — | —"
        ));
        assert!(
            output.find("step=activity").unwrap() < output.find("step=withdrawal").unwrap(),
            "receipt metric groups should preserve report order"
        );
    }

    #[test]
    fn rejects_a_report_that_does_not_match_the_rendered_scenario() {
        let mismatch = SCENARIO.replace("save: activity", "save: transfer");
        let error = render_results(REPORT, Some(&mismatch)).unwrap_err();
        assert!(error.to_string().contains("names it transfer"));
    }

    #[test]
    fn requires_scenario_yaml_for_scenario_reports() {
        let error = render_results(REPORT, None).unwrap_err();
        assert!(error.to_string().contains("--scenario is required"));
    }

    #[test]
    fn accepts_optional_step_names_and_timeouts() {
        let scenario = SCENARIO.replace("      save: withdrawal", "      timeout: 2s");
        let report = REPORT.replace("\"name\": \"withdrawal\"", "\"name\": \"step_4_submit\"");
        let output = render_results(&report, Some(&scenario)).unwrap();

        assert!(output.contains("`step_4_submit` | `zone`"));
    }

    #[test]
    fn renders_scenario_failure_classifications_without_sample_details() {
        let report = REPORT.replace(
            "\"timed_out\": 0,",
            "\"timed_out\": 1, \"failures\": [{\"step_index\": 3, \"step_name\": \"withdrawal\", \"classification\": \"timeout\", \"count\": 1, \"sample_detail\": \"secret\"}],",
        );
        let output = render_results(&report, Some(SCENARIO)).unwrap();

        assert!(output.contains("`withdrawal` (# 3) | `timeout` | 1"));
        assert!(!output.contains("secret"));
    }

    #[test]
    fn keeps_chains_whose_submit_steps_all_failed() {
        let mut report: serde_json::Value = serde_json::from_str(REPORT).unwrap();
        for step in report["steps"].as_array_mut().unwrap() {
            if step["index"].as_u64().is_some_and(|index| index >= 2) {
                step["success"] = 0.into();
            }
        }
        let output =
            render_results(&serde_json::to_string(&report).unwrap(), Some(SCENARIO)).unwrap();

        assert!(
            output.contains(
                "`zone` successful user submit steps (chain ID 421700001) | 0 | 0.000 TPS"
            )
        );
    }

    #[test]
    fn renders_an_independent_phase_report() {
        let report = r#"{
          "sent": 100,
          "success": 99,
          "failed": 100,
          "elapsed_secs": 10.0,
          "tps": 10.0,
          "success_rate": 99.0,
          "latency": {
            "min_ms": 0.1,
            "max_ms": 2.0,
            "mean_ms": 0.5,
            "p50_ms": 0.4,
            "p95_ms": 1.0,
            "p99_ms": 2.0
          }
        }"#;
        let output = render_results(report, None).unwrap();

        assert!(output.contains("Attempted transactions | **10.000 TPS**"));
        assert!(output.contains("RPC-accepted transactions | **9.900 TPS**"));
        assert!(output.contains("| 100 | 99 | 100 | 99.000% | 10.000 s |"));
        assert!(!output.contains("Receipt gas metrics"));
    }

    #[test]
    fn renders_receipt_metrics_from_a_phase_report() {
        let report = r#"{
          "sent": 1,
          "success": 1,
          "failed": 0,
          "elapsed_secs": 1.0,
          "tps": 1.0,
          "success_rate": 100.0,
          "latency": null,
          "receipt_metrics": [{
            "labels": {"input": "transfer"},
            "gas_used": {"count": 1, "min": 21000.0, "mean": 21000.0, "p50": 21000.0, "p95": 21000.0, "p99": 21000.0},
            "effective_gas_price": {"count": 1, "min": 7.0, "mean": 7.0, "p50": 7.0, "p95": 7.0, "p99": 7.0},
            "fee_paid": {"count": 1, "min": 147000.0, "mean": 147000.0, "p50": 147000.0, "p95": 147000.0, "p99": 147000.0}
          }]
        }"#;

        let output = render_results(report, None).unwrap();

        assert!(output.contains(
            "`input=transfer` | `effective_gas_price` | 1 | 7.000 wei | 7.000 wei | 7.000 wei | 7.000 wei | 7.000 wei"
        ));
    }

    #[test]
    fn rejects_zero_duration_reports() {
        let report = r#"{
          "sent": 0,
          "success": 0,
          "failed": 0,
          "elapsed_secs": 0.0,
          "tps": 0.0,
          "success_rate": 0.0,
          "latency": null
        }"#;
        let error = render_results(report, None).unwrap_err();
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn escapes_dynamic_markdown_cells() {
        assert_eq!(code("a|b`c\nd"), "`a&#124;b&#96;c d`");
    }
}
