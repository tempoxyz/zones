use eyre::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

const BUILTIN_CATALOG: &str = include_str!("../../../contrib/loadtest/catalog.yml");

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Catalog {
    pub(crate) version: u32,
    pub(crate) profiles: BTreeMap<String, Profile>,
    pub(crate) cases: BTreeMap<String, Case>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Profile {
    pub(crate) description: String,
    pub(crate) execution: ExecutionMode,
    pub(crate) default_count: u64,
    pub(crate) cases: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionMode {
    Sequential,
    Parallel,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Case {
    pub(crate) description: String,
    #[serde(default)]
    pub(crate) scenario: Option<PathBuf>,
    #[serde(default = "default_true")]
    pub(crate) implemented: bool,
    #[serde(default)]
    pub(crate) dangerous: bool,
    #[serde(default)]
    pub(crate) mutation_group: Option<String>,
    #[serde(default = "default_weight")]
    pub(crate) weight: u64,
    #[serde(default)]
    pub(crate) covers: Vec<String>,
    #[serde(default)]
    pub(crate) requires: Vec<String>,
}

const fn default_true() -> bool {
    true
}

const fn default_weight() -> u64 {
    1
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RunPlan {
    pub(crate) version: u32,
    pub(crate) profile: String,
    pub(crate) execution: ExecutionMode,
    pub(crate) seed: u64,
    pub(crate) requested_count: Option<u64>,
    pub(crate) duration: Option<String>,
    #[serde(default)]
    pub(crate) forever: bool,
    pub(crate) starts_per_second: f64,
    pub(crate) transactions_per_second: u64,
    pub(crate) max_in_flight: usize,
    pub(crate) max_rpc_in_flight: usize,
    pub(crate) cases: Vec<CasePlan>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CasePlan {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) scenario: PathBuf,
    pub(crate) seed: u64,
    pub(crate) count: Option<u64>,
    pub(crate) duration: Option<String>,
    pub(crate) starts_per_second: f64,
    pub(crate) transactions_per_second: u64,
    pub(crate) max_in_flight: usize,
    pub(crate) max_rpc_in_flight: usize,
    pub(crate) dangerous: bool,
    pub(crate) mutation_group: Option<String>,
    pub(crate) covers: Vec<String>,
    pub(crate) requires: Vec<String>,
}

pub(crate) struct PlanRequest<'a> {
    pub(crate) profile: &'a str,
    pub(crate) selected_cases: &'a [String],
    pub(crate) scenario_root: &'a Path,
    pub(crate) seed: u64,
    pub(crate) count: Option<u64>,
    pub(crate) duration: Option<String>,
    pub(crate) forever: bool,
    pub(crate) starts_per_second: f64,
    pub(crate) transactions_per_second: u64,
    pub(crate) max_in_flight: usize,
    pub(crate) max_rpc_in_flight: usize,
}

impl Catalog {
    pub(crate) fn load(path: Option<&Path>) -> Result<Self> {
        let contents = match path {
            Some(path) => fs::read_to_string(path)
                .wrap_err_with(|| format!("failed reading catalog {}", path.display()))?,
            None => BUILTIN_CATALOG.to_owned(),
        };
        let catalog: Self =
            serde_yaml::from_str(&contents).wrap_err("failed parsing load-test catalog")?;
        catalog.validate()?;
        Ok(catalog)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.version == 1,
            "unsupported catalog version {}",
            self.version
        );
        ensure!(!self.profiles.is_empty(), "catalog has no profiles");
        ensure!(!self.cases.is_empty(), "catalog has no cases");

        for (name, profile) in &self.profiles {
            ensure!(!profile.cases.is_empty(), "profile {name} has no cases");
            ensure!(
                profile.default_count > 0,
                "profile {name} has a zero default count"
            );
            let mut seen = BTreeSet::new();
            for case in &profile.cases {
                ensure!(
                    self.cases.contains_key(case),
                    "profile {name} references unknown case {case}"
                );
                ensure!(seen.insert(case), "profile {name} repeats case {case}");
            }
        }

        for (name, case) in &self.cases {
            ensure!(
                !case.covers.is_empty(),
                "case {name} has no coverage labels"
            );
            if case.implemented {
                ensure!(
                    case.scenario.is_some(),
                    "implemented case {name} has no scenario"
                );
                ensure!(case.weight > 0, "implemented case {name} has zero weight");
            }
        }
        Ok(())
    }

    pub(crate) fn plan(&self, request: PlanRequest<'_>) -> Result<RunPlan> {
        ensure!(
            request.max_in_flight > 0,
            "--max-in-flight must be at least 1"
        );
        ensure!(
            request.max_rpc_in_flight > 0,
            "--max-rpc-in-flight must be at least 1"
        );
        ensure!(
            request.starts_per_second.is_finite() && request.starts_per_second >= 0.0,
            "--journeys-per-second must be finite and non-negative"
        );
        let profile = self
            .profiles
            .get(request.profile)
            .ok_or_else(|| eyre::eyre!("unknown profile {}", request.profile))?;
        let ids = if request.selected_cases.is_empty() {
            profile.cases.clone()
        } else {
            request.selected_cases.to_vec()
        };
        ensure!(!ids.is_empty(), "no cases selected");

        let mut selected = Vec::with_capacity(ids.len());
        let mut seen = BTreeSet::new();
        for id in ids {
            ensure!(seen.insert(id.clone()), "case {id} selected more than once");
            let case = self
                .cases
                .get(&id)
                .ok_or_else(|| eyre::eyre!("unknown case {id}"))?;
            if !case.implemented {
                bail!("case {id} is catalogued but not implemented");
            }
            selected.push((id, case));
        }

        if (request.duration.is_some() || request.forever)
            && profile.execution == ExecutionMode::Sequential
            && selected.len() > 1
        {
            let option = if request.forever {
                "--forever"
            } else {
                "--duration"
            };
            bail!("{option} with multiple cases requires a parallel profile or a single --case");
        }

        let total_count = if request.duration.is_some() {
            None
        } else if request.forever {
            Some(u64::MAX)
        } else {
            Some(request.count.unwrap_or(profile.default_count))
        };
        let weights = selected
            .iter()
            .map(|(_, case)| case.weight)
            .collect::<Vec<_>>();
        let counts = total_count
            .map(|total| allocate_counts(total, &weights, request.seed))
            .transpose()?;
        let starts = allocate_rate(request.starts_per_second, &weights, profile.execution);
        let transactions = allocate_transaction_rate(
            request.transactions_per_second,
            &weights,
            profile.execution,
            request.seed ^ 0x7478_5f72_6174_6500,
        )?;
        let in_flight = allocate_limit(
            request.max_in_flight,
            &weights,
            profile.execution,
            request.seed ^ 0x696e_666c_6967_6874,
        )?;
        let rpc_in_flight = allocate_limit(
            request.max_rpc_in_flight,
            &weights,
            profile.execution,
            request.seed ^ 0x7270_635f_6c69_6d69,
        )?;

        let cases = selected
            .into_iter()
            .enumerate()
            .map(|(index, (id, case))| {
                let scenario = request.scenario_root.join(
                    case.scenario
                        .as_ref()
                        .expect("implemented cases have scenario paths"),
                );
                let mut requires = case.requires.iter().cloned().collect::<BTreeSet<_>>();
                requires.extend(discover_env_requirements(&scenario)?);
                Ok(CasePlan {
                    scenario,
                    seed: derive_seed(request.seed, &id),
                    count: counts.as_ref().map(|counts| counts[index]),
                    duration: request.duration.clone(),
                    starts_per_second: starts[index],
                    transactions_per_second: transactions[index],
                    max_in_flight: in_flight[index],
                    max_rpc_in_flight: rpc_in_flight[index],
                    id,
                    description: case.description.clone(),
                    dangerous: case.dangerous,
                    mutation_group: case.mutation_group.clone(),
                    covers: case.covers.clone(),
                    requires: requires.into_iter().collect(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        Ok(RunPlan {
            version: 1,
            profile: request.profile.to_owned(),
            execution: profile.execution,
            seed: request.seed,
            requested_count: if request.forever { None } else { total_count },
            duration: request.duration,
            forever: request.forever,
            starts_per_second: request.starts_per_second,
            transactions_per_second: request.transactions_per_second,
            max_in_flight: request.max_in_flight,
            max_rpc_in_flight: request.max_rpc_in_flight,
            cases,
        })
    }

    pub(crate) fn all_coverage_labels(&self) -> BTreeSet<String> {
        self.cases
            .values()
            .flat_map(|case| case.covers.iter().cloned())
            .collect()
    }

    pub(crate) fn implemented_coverage_labels(&self) -> BTreeSet<String> {
        self.cases
            .values()
            .filter(|case| case.implemented)
            .flat_map(|case| case.covers.iter().cloned())
            .collect()
    }
}

fn discover_env_requirements(root: &Path) -> Result<BTreeSet<String>> {
    fn visit(
        path: &Path,
        visited: &mut BTreeSet<PathBuf>,
        requirements: &mut BTreeSet<String>,
    ) -> Result<()> {
        let canonical = path
            .canonicalize()
            .wrap_err_with(|| format!("failed resolving scenario dependency {}", path.display()))?;
        if !visited.insert(canonical.clone()) {
            return Ok(());
        }
        let contents = fs::read_to_string(&canonical).wrap_err_with(|| {
            format!("failed reading scenario dependency {}", canonical.display())
        })?;
        extract_env_names(&contents, requirements);

        let parent = canonical.parent().unwrap_or_else(|| Path::new("."));
        for dependency in yaml_dependencies(&contents) {
            visit(&parent.join(dependency), visited, requirements)?;
        }
        Ok(())
    }

    let mut visited = BTreeSet::new();
    let mut requirements = BTreeSet::new();
    visit(root, &mut visited, &mut requirements)?;
    Ok(requirements)
}

fn yaml_dependencies(contents: &str) -> Vec<String> {
    let mut dependencies = Vec::new();
    let mut include_indent = None;
    for line in contents.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "include:" {
            include_indent = Some(indent);
            continue;
        }
        if let Some(base) = include_indent {
            if indent > base {
                if let Some(value) = trimmed.strip_prefix("- ") {
                    dependencies.push(unquote(value).to_owned());
                }
                continue;
            }
            include_indent = None;
        }
        if let Some(value) = trimmed.strip_prefix("workload:") {
            let value = value.trim();
            if !value.is_empty() {
                dependencies.push(unquote(value).to_owned());
            }
        }
    }
    dependencies
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn extract_env_names(contents: &str, requirements: &mut BTreeSet<String>) {
    let mut remaining = contents;
    while let Some(start) = remaining.find("${") {
        remaining = &remaining[start + 2..];
        let Some(end) = remaining.find('}') else {
            break;
        };
        let name = &remaining[..end];
        if !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte == b'_' || byte.is_ascii_alphanumeric())
        {
            requirements.insert(name.to_owned());
        }
        remaining = &remaining[end + 1..];
    }
}

fn allocate_counts(total: u64, weights: &[u64], seed: u64) -> Result<Vec<u64>> {
    ensure!(!weights.is_empty(), "cannot allocate count to no cases");
    ensure!(
        total >= weights.len() as u64,
        "count {total} is smaller than the {} selected cases; every case must run once",
        weights.len()
    );
    let weight_sum = weights.iter().copied().sum::<u64>();
    ensure!(weight_sum > 0, "selected cases have zero total weight");

    let remaining = total - weights.len() as u64;
    let mut counts = vec![1; weights.len()];
    let mut assigned = 0;
    let mut remainders = Vec::with_capacity(weights.len());
    for (index, weight) in weights.iter().copied().enumerate() {
        let numerator = u128::from(remaining) * u128::from(weight);
        let share = (numerator / u128::from(weight_sum)) as u64;
        counts[index] += share;
        assigned += share;
        remainders.push((
            index,
            numerator % u128::from(weight_sum),
            splitmix64(seed ^ index as u64),
        ));
    }

    remainders.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| right.2.cmp(&left.2))
            .then_with(|| left.0.cmp(&right.0))
    });
    for (index, _, _) in remainders.into_iter().take((remaining - assigned) as usize) {
        counts[index] += 1;
    }
    Ok(counts)
}

fn allocate_rate(total: f64, weights: &[u64], mode: ExecutionMode) -> Vec<f64> {
    if total == 0.0 || mode == ExecutionMode::Sequential {
        return vec![total; weights.len()];
    }
    let weight_sum = weights.iter().sum::<u64>() as f64;
    weights
        .iter()
        .map(|weight| total * *weight as f64 / weight_sum)
        .collect()
}

fn allocate_transaction_rate(
    total: u64,
    weights: &[u64],
    mode: ExecutionMode,
    seed: u64,
) -> Result<Vec<u64>> {
    if total == 0 || mode == ExecutionMode::Sequential {
        return Ok(vec![total; weights.len()]);
    }
    allocate_counts(total, weights, seed).wrap_err_with(|| {
        format!(
            "cannot allocate --transactions-per-second {total} across {} parallel cases",
            weights.len()
        )
    })
}

fn allocate_limit(
    total: usize,
    weights: &[u64],
    mode: ExecutionMode,
    seed: u64,
) -> Result<Vec<usize>> {
    if mode == ExecutionMode::Sequential {
        return Ok(vec![total; weights.len()]);
    }
    allocate_counts(total as u64, weights, seed)
        .map(|counts| counts.into_iter().map(|count| count as usize).collect())
}

fn derive_seed(master: u64, id: &str) -> u64 {
    let hash = id.bytes().fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x1000_0000_01b3)
    });
    splitmix64(master ^ hash)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

pub(crate) fn compare_coverage_status(left: &str, right: &str) -> Ordering {
    coverage_status_rank(left)
        .cmp(&coverage_status_rank(right))
        .then_with(|| left.cmp(right))
}

fn coverage_status_rank(status: &str) -> u8 {
    match status {
        "failed" => 0,
        "uncovered" => 1,
        "covered" => 2,
        _ => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_catalog_is_valid() {
        Catalog::load(None).unwrap();
    }

    #[test]
    fn count_allocation_is_complete_and_deterministic() {
        let first = allocate_counts(101, &[30, 35, 15, 15, 5], 42).unwrap();
        let second = allocate_counts(101, &[30, 35, 15, 15, 5], 42).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.iter().sum::<u64>(), 101);
        assert!(first.iter().all(|count| *count >= 1));
    }

    #[test]
    fn every_selected_case_runs_at_least_once() {
        assert!(allocate_counts(1, &[1, 1], 1).is_err());
        assert_eq!(allocate_counts(2, &[100, 1], 1).unwrap(), vec![1, 1]);
    }

    #[test]
    fn parallel_transaction_rate_is_integral_and_preserves_the_total() {
        let rates =
            allocate_transaction_rate(11, &[30, 30, 40], ExecutionMode::Parallel, 42).unwrap();
        assert_eq!(rates.iter().sum::<u64>(), 11);
        assert!(rates.iter().all(|rate| *rate >= 1));
        assert!(allocate_transaction_rate(2, &[1, 1, 1], ExecutionMode::Parallel, 42).is_err());
        assert_eq!(
            allocate_transaction_rate(0, &[1, 1], ExecutionMode::Parallel, 42).unwrap(),
            vec![0, 0]
        );
    }

    #[test]
    fn forever_plan_uses_parallel_maximum_counts() {
        let catalog = Catalog::load(None).unwrap();
        let selected = vec![
            "encrypted-deposit".to_owned(),
            "plain-withdrawal".to_owned(),
        ];
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let plan = catalog
            .plan(PlanRequest {
                profile: "steady",
                selected_cases: &selected,
                scenario_root: &root,
                seed: 42,
                count: None,
                duration: None,
                forever: true,
                starts_per_second: 5.0,
                transactions_per_second: 50,
                max_in_flight: 20,
                max_rpc_in_flight: 100,
            })
            .unwrap();

        assert!(plan.forever);
        assert_eq!(plan.requested_count, None);
        assert_eq!(
            plan.cases
                .iter()
                .map(|case| u128::from(case.count.unwrap()))
                .sum::<u128>(),
            u128::from(u64::MAX)
        );
    }

    #[test]
    fn forever_rejects_multiple_sequential_cases() {
        let catalog = Catalog::load(None).unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let error = catalog
            .plan(PlanRequest {
                profile: "core",
                selected_cases: &[],
                scenario_root: &root,
                seed: 1,
                count: None,
                duration: None,
                forever: true,
                starts_per_second: 1.0,
                transactions_per_second: 10,
                max_in_flight: 2,
                max_rpc_in_flight: 2,
            })
            .unwrap_err();

        assert!(error.to_string().contains("--forever with multiple cases"));
    }

    #[test]
    fn derived_seeds_are_stable_and_case_specific() {
        assert_eq!(derive_seed(42, "deposit"), derive_seed(42, "deposit"));
        assert_ne!(derive_seed(42, "deposit"), derive_seed(42, "withdrawal"));
    }

    #[test]
    fn discovers_requirements_through_scenario_dependencies() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../contrib/bench/neobank/encrypted-deposit-scenario.yml");
        let requirements = discover_env_requirements(&root).unwrap();
        assert!(requirements.contains("L1_RPC_URL"));
        assert!(requirements.contains("ZONES_BENCH_SEQUENCER_ACCOUNT_INDEX"));
        assert!(requirements.contains("ZONES_BENCH_REWARD_FUND_AMOUNT"));
    }
}
