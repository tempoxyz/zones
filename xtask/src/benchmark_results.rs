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
    receipt_metrics: Vec<ReceiptMetricReport>,
    #[serde(default)]
    failures: Vec<FailureReport>,
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
}

#[derive(Debug, Deserialize)]
struct StepReport {
    index: usize,
    name: String,
    kind: String,
    success: u64,
    failed: u64,
    latency: Latency,
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
            format_millis(step.report.latency.p50_ms),
            format_millis(step.report.latency.p95_ms),
        )?;
    }
    writeln!(output)?;

    writeln!(output, "## Completed journey latency\n")?;
    write_latency_header(&mut output)?;
    write_latency_row(&mut output, &report.total_scenario_latency)?;
    writeln!(output)?;

    writeln!(output, "## Measured step latency\n")?;
    writeln!(
        output,
        "| Step | Chain | Operation | Successful | Failed | Mean | P50 | P95 | P99 |"
    )?;
    writeln!(
        output,
        "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |"
    )?;
    for step in steps.iter().filter(|step| step.report.kind != "checkpoint") {
        writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            code(&step.report.name),
            code(step.chain),
            code(&step.report.kind),
            step.report.success,
            step.report.failed,
            format_millis(step.report.latency.mean_ms),
            format_millis(step.report.latency.p50_ms),
            format_millis(step.report.latency.p95_ms),
            format_millis(step.report.latency.p99_ms),
        )?;
    }
    writeln!(output)?;

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
        "> Effective rates use the complete measured window, including ramp-up and drain. Aggregate user TPS sums submit steps across chains and is not a single-chain saturation result. Setup outside the measured scenario is excluded. A submit step ends at RPC acceptance by default, or at a successful receipt when configured with `await: receipt`; receipt and log wait steps capture subsequent execution and cross-chain progress."
    )?;

    Ok(output)
}

fn validate_scenario_report(report: &ScenarioReport, scenario: &ScenarioSpec) -> Result<()> {
    ensure!(
        report.version == scenario.version,
        "report version {} does not match scenario version {}",
        report.version,
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

    let mut names = BTreeSet::new();
    for chain in &report.configuration.chains {
        ensure!(
            names.insert(chain.name.as_str()),
            "duplicate report chain {}",
            chain.name
        );
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
            validate_latency(
                &format!("step {} latency", reported.name),
                &reported.latency,
            )?;
            Ok(ResolvedStep {
                report: reported,
                chain,
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
    if value >= 1_000.0 {
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
        assert!(output.contains("`deposit_processed` | `zone` | `wait_log`"));
        assert!(output.contains("4.100 s"));
        assert!(!output.contains("Receipt gas metrics"));
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
