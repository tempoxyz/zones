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
    save: String,
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
        format_seconds(elapsed_secs),
    )?;
    writeln!(
        output,
        "Offered load: **{:.3} journey starts/s**. Maximum in flight: **{} observed / {} configured**.\n",
        report.configuration.starts_per_second,
        report.maximum_in_flight,
        report.configuration.maximum_in_flight,
    )?;

    writeln!(output, "## Throughput\n")?;
    writeln!(output, "| Scope | Successful operations | Effective rate |")?;
    writeln!(output, "| --- | ---: | ---: |")?;
    writeln!(
        output,
        "| Complete journeys | {} | **{:.3} journeys/s** |",
        report.completed, report.completed_scenarios_per_second,
    )?;
    writeln!(
        output,
        "| All submitted user transactions | {submitted} | **{aggregate_tps:.3} TPS** |",
    )?;
    for chain in &report.configuration.chains {
        let successes = submit_steps
            .iter()
            .filter(|step| step.chain == chain.name)
            .try_fold(0_u64, |total, step| {
                total
                    .checked_add(step.report.success)
                    .ok_or_else(|| eyre::eyre!("successful chain submit count overflow"))
            })?;
        if successes == 0 {
            continue;
        }
        writeln!(
            output,
            "| {} user transactions (chain ID {}) | {} | {:.3} TPS |",
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
        "| Step | Chain | Successful | Failed | Effective TPS | RPC submit p50 | RPC submit p95 |"
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

    writeln!(output, "## End-to-end journey latency\n")?;
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

    writeln!(
        output,
        "> Effective rates use the complete measured window, including ramp-up and drain. Aggregate user TPS sums submissions across chains and is not a single-chain saturation result. Setup outside the measured scenario is excluded. Submit latency ends at RPC acceptance; receipt and log waits capture subsequent execution and cross-chain progress."
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
                reported.name == rendered.save,
                "report step {} is named {} but rendered scenario saves it as {}",
                index,
                reported.name,
                rendered.save
            );
            ensure!(
                rendered.operation.len() == 1,
                "rendered scenario step {} must contain exactly one operation",
                rendered.save
            );
            let (kind, body) = rendered.operation.iter().next().expect("length checked");
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
                    rendered.save
                )
            })?;
            let chain = body
                .get(serde_yaml::Value::String("chain".to_owned()))
                .and_then(serde_yaml::Value::as_str)
                .ok_or_else(|| {
                    eyre::eyre!("rendered scenario step {} has no chain", rendered.save)
                })?;
            ensure!(
                scenario.chains.contains_key(chain),
                "rendered scenario step {} references unknown chain {}",
                rendered.save,
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
    validate_nonnegative_finite("phase success rate", report.success_rate)?;
    ensure!(
        report.success <= report.sent,
        "successful count exceeds sent count"
    );
    ensure!(
        report.failed <= report.sent,
        "failed count exceeds sent count"
    );
    ensure!(
        report.success <= report.sent - report.failed,
        "resolved count exceeds sent count"
    );

    let mut output = String::new();
    writeln!(output, "# Zones benchmark results\n")?;
    writeln!(output, "## Outcome\n")?;
    writeln!(
        output,
        "| Sent | Successful | Failed | Success rate | Measured time |"
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
        "| Successful transactions | **{:.3} TPS** |\n",
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
            report.sent,
            format_millis(latency.min_ms),
            format_millis(latency.mean_ms),
            format_millis(latency.p50_ms),
            format_millis(latency.p95_ms),
            format_millis(latency.p99_ms),
            format_millis(latency.max_ms)
        )?;
    }

    writeln!(
        output,
        "> Rates use the complete measured window, including ramp-up and drain. Latency ends when the RPC returns; setup transactions are excluded from the measured phase report."
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
        assert!(output.contains("All submitted user transactions | 299 | **14.950 TPS**"));
        assert!(output.contains("`l1` user transactions (chain ID 1337) | 100 | 5.000 TPS"));
        assert!(output.contains("`zone` user transactions (chain ID 421700001) | 199 | 9.950 TPS"));
        assert!(output.contains("`withdrawal` | `zone` | 99 | 1 | 4.950"));
        assert!(output.contains("`deposit_processed` | `zone` | `wait_log`"));
        assert!(output.contains("4.100 s"));
    }

    #[test]
    fn rejects_a_report_that_does_not_match_the_rendered_scenario() {
        let mismatch = SCENARIO.replace("save: activity", "save: transfer");
        let error = render_results(REPORT, Some(&mismatch)).unwrap_err();
        assert!(error.to_string().contains("saves it as transfer"));
    }

    #[test]
    fn requires_scenario_yaml_for_scenario_reports() {
        let error = render_results(REPORT, None).unwrap_err();
        assert!(error.to_string().contains("--scenario is required"));
    }

    #[test]
    fn renders_an_independent_phase_report() {
        let report = r#"{
          "sent": 100,
          "success": 99,
          "failed": 1,
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
        assert!(output.contains("Successful transactions | **9.900 TPS**"));
        assert!(output.contains("| 100 | 99 | 1 | 99.000% | 10.000 s |"));
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
