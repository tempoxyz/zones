use crate::{
    catalog::{CasePlan, Catalog, RunPlan, compare_coverage_status},
    deployment::DeploymentContext,
};
use eyre::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RunReport {
    pub(crate) version: u32,
    pub(crate) run_id: String,
    pub(crate) profile: String,
    pub(crate) seed: u64,
    pub(crate) started_at_unix_ms: u128,
    pub(crate) elapsed_ms: u128,
    pub(crate) cancelled: bool,
    pub(crate) txgen_bin: String,
    pub(crate) failure_policy: String,
    pub(crate) step_timeout: String,
    #[serde(default)]
    pub(crate) funded_accounts: usize,
    pub(crate) deployment: DeploymentContext,
    pub(crate) plan: RunPlan,
    pub(crate) cases: Vec<CaseResult>,
    pub(crate) coverage: CoverageReport,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CaseResult {
    pub(crate) plan: CasePlan,
    pub(crate) status: CaseStatus,
    pub(crate) exit_code: Option<i32>,
    pub(crate) elapsed_ms: u128,
    pub(crate) txgen_report: PathBuf,
    #[serde(default)]
    pub(crate) txgen: Option<TxgenSummary>,
    #[serde(default)]
    pub(crate) error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CaseStatus {
    Passed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct TxgenSummary {
    #[serde(default)]
    pub(crate) scenario: String,
    #[serde(default)]
    pub(crate) elapsed_ms: u64,
    #[serde(default)]
    pub(crate) started: u64,
    #[serde(default)]
    pub(crate) completed: u64,
    #[serde(default)]
    pub(crate) failed: u64,
    #[serde(default)]
    pub(crate) timed_out: u64,
    #[serde(default)]
    pub(crate) completed_scenarios_per_second: f64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CoverageReport {
    pub(crate) selected_total: usize,
    pub(crate) selected_covered: usize,
    pub(crate) selected_percent: f64,
    pub(crate) implemented_total: usize,
    pub(crate) implemented_covered: usize,
    pub(crate) implemented_percent: f64,
    pub(crate) catalog_total: usize,
    pub(crate) entries: Vec<CoverageEntry>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct CoverageEntry {
    pub(crate) label: String,
    pub(crate) status: String,
    pub(crate) implemented: bool,
    pub(crate) cases: Vec<String>,
}

#[derive(Debug, Serialize)]
struct JournalEvent<'a> {
    timestamp_unix_ms: u128,
    event: &'a str,
    run_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    case: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<Value>,
}

pub(crate) struct Journal {
    writer: BufWriter<File>,
    run_id: String,
}

impl Journal {
    pub(crate) fn create(path: &Path, run_id: &str) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).wrap_err_with(|| {
                format!("failed creating journal directory {}", parent.display())
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .wrap_err_with(|| format!("failed opening journal {}", path.display()))?;
        Ok(Self {
            writer: BufWriter::new(file),
            run_id: run_id.to_owned(),
        })
    }

    pub(crate) fn record(
        &mut self,
        event: &str,
        case: Option<&str>,
        detail: Option<Value>,
    ) -> Result<()> {
        let entry = JournalEvent {
            timestamp_unix_ms: unix_ms(),
            event,
            run_id: &self.run_id,
            case,
            detail,
        };
        serde_json::to_writer(&mut self.writer, &entry)
            .wrap_err("failed encoding journal event")?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
}

impl CoverageReport {
    pub(crate) fn build(catalog: &Catalog, plan: &RunPlan, results: &[CaseResult]) -> Self {
        let selected = plan
            .cases
            .iter()
            .flat_map(|case| case.covers.iter().cloned())
            .collect::<BTreeSet<_>>();
        let covered = results
            .iter()
            .filter(|result| result.status == CaseStatus::Passed)
            .flat_map(|result| result.plan.covers.iter().cloned())
            .collect::<BTreeSet<_>>();
        let failed = results
            .iter()
            .filter(|result| result.status != CaseStatus::Passed)
            .flat_map(|result| result.plan.covers.iter().cloned())
            .collect::<BTreeSet<_>>();
        let implemented = catalog.implemented_coverage_labels();
        let all = catalog.all_coverage_labels();

        let mut owners = BTreeMap::<String, Vec<String>>::new();
        for (id, case) in &catalog.cases {
            for label in &case.covers {
                owners.entry(label.clone()).or_default().push(id.clone());
            }
        }

        let mut entries = all
            .iter()
            .map(|label| CoverageEntry {
                status: if covered.contains(label) {
                    "covered"
                } else if failed.contains(label) {
                    "failed"
                } else {
                    "uncovered"
                }
                .to_owned(),
                implemented: implemented.contains(label),
                cases: owners.remove(label).unwrap_or_default(),
                label: label.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            compare_coverage_status(&left.status, &right.status)
                .then_with(|| left.label.cmp(&right.label))
        });

        let selected_covered = selected.intersection(&covered).count();
        let implemented_covered = implemented.intersection(&covered).count();
        Self {
            selected_total: selected.len(),
            selected_covered,
            selected_percent: percent(selected_covered, selected.len()),
            implemented_total: implemented.len(),
            implemented_covered,
            implemented_percent: percent(implemented_covered, implemented.len()),
            catalog_total: all.len(),
            entries,
        }
    }
}

pub(crate) fn load_txgen_summary(path: &Path) -> Result<TxgenSummary> {
    let contents = fs::read_to_string(path)
        .wrap_err_with(|| format!("failed reading txgen report {}", path.display()))?;
    serde_json::from_str(&contents)
        .wrap_err_with(|| format!("failed parsing txgen report {}", path.display()))
}

pub(crate) fn load_run_report(path: &Path) -> Result<RunReport> {
    let contents = fs::read_to_string(path)
        .wrap_err_with(|| format!("failed reading run report {}", path.display()))?;
    serde_json::from_str(&contents)
        .wrap_err_with(|| format!("failed parsing run report {}", path.display()))
}

pub(crate) fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("failed creating report directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("report.json");
    let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let contents = serde_json::to_vec_pretty(value).wrap_err("failed encoding JSON report")?;
    fs::write(&temporary, contents)
        .wrap_err_with(|| format!("failed writing temporary report {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .wrap_err_with(|| format!("failed publishing report {}", path.display()))?;
    Ok(())
}

pub(crate) fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
}

fn percent(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        100.0
    } else {
        numerator as f64 * 100.0 / denominator as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ExecutionMode, RunPlan};

    #[test]
    fn failed_case_does_not_claim_coverage() {
        let catalog = Catalog::load(None).unwrap();
        let case = catalog.cases.get("encrypted-deposit").unwrap();
        let plan_case = CasePlan {
            id: "encrypted-deposit".to_owned(),
            description: case.description.clone(),
            scenario: PathBuf::from("scenario.yml"),
            seed: 1,
            count: Some(1),
            duration: None,
            starts_per_second: 0.0,
            transactions_per_second: 0,
            max_in_flight: 1,
            max_rpc_in_flight: 1,
            dangerous: false,
            mutation_group: None,
            covers: case.covers.clone(),
            requires: case.requires.clone(),
        };
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
            cases: vec![plan_case.clone()],
        };
        let result = CaseResult {
            plan: plan_case,
            status: CaseStatus::Failed,
            exit_code: Some(1),
            elapsed_ms: 1,
            txgen_report: PathBuf::from("missing.json"),
            txgen: None,
            error: Some("test".to_owned()),
        };
        let coverage = CoverageReport::build(&catalog, &plan, &[result]);
        assert_eq!(coverage.selected_covered, 0);
        assert!(
            coverage
                .entries
                .iter()
                .any(|entry| entry.status == "failed")
        );
    }
}
