//! Explicit Tempo fork schedules for newly generated Zone genesis files.

use alloy::genesis::ChainConfig;
use eyre::{WrapErr as _, ensure};
use std::{collections::BTreeMap, fs};
use tempo_chainspec::hardfork::TempoHardfork;

/// Fork schedule arguments shared by both genesis-producing commands.
#[derive(Debug, Default, clap::Args)]
pub(crate) struct GenesisForkArgs {
    /// JSON file mapping Tempo genesis fields (e.g. t4Time) to u64 timestamps.
    /// Replaces the bundled schedule; T12/T13 flags override its entries.
    #[arg(long, value_name = "PATH", value_parser = GenesisForkSchedule::from_file)]
    fork_schedule: Option<GenesisForkSchedule>,

    /// Override the T12 activation timestamp inherited from L1.
    #[arg(long)]
    t12_time: Option<u64>,

    /// Override the T13 activation timestamp inherited from L1.
    #[arg(long)]
    t13_time: Option<u64>,
}

impl GenesisForkArgs {
    pub(crate) fn apply_to(&self, config: &mut ChainConfig) -> eyre::Result<()> {
        let mut schedule = match &self.fork_schedule {
            Some(schedule) => schedule.clone(),
            None => GenesisForkSchedule::parse(include_str!("../genesis-forks.json"))?,
        };
        for (name, timestamp) in [("t12Time", self.t12_time), ("t13Time", self.t13_time)] {
            if let Some(timestamp) = timestamp {
                schedule.0.insert(name.to_owned(), timestamp);
            }
        }
        for (name, timestamp) in schedule.0 {
            config.extra_fields.insert_value(name, timestamp)?;
        }
        Ok(())
    }
}

/// A validated schedule, read during CLI parsing before any onchain action.
#[derive(Clone, Debug)]
struct GenesisForkSchedule(BTreeMap<String, u64>);

impl GenesisForkSchedule {
    fn from_file(path: &str) -> eyre::Result<Self> {
        let json = fs::read_to_string(path)
            .wrap_err_with(|| format!("failed reading fork schedule {path}"))?;
        Self::parse(&json).wrap_err_with(|| format!("invalid fork schedule {path}"))
    }

    fn parse(json: &str) -> eyre::Result<Self> {
        let schedule = serde_json::from_str::<BTreeMap<String, u64>>(json)?;
        for name in schedule.keys() {
            ensure!(
                TempoHardfork::VARIANTS.iter().any(|fork| {
                    *fork != TempoHardfork::Genesis
                        && *name == format!("{}Time", fork.to_string().to_lowercase())
                }),
                "unknown Tempo fork field {name}"
            );
        }
        Ok(Self(schedule))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_invalid_schedule_fields_and_timestamps() {
        for json in [
            r#"{"t4time":0}"#,
            r#"{"genesisTime":0}"#,
            r#"{"t999Time":0}"#,
            r#"{"t4Time":-1}"#,
            r#"{"t4Time":1.5}"#,
            r#"{"t4Time":18446744073709551616}"#,
            r#"{"t4Time":null}"#,
            r#"{"t4Time":"0"}"#,
            "[]",
        ] {
            assert!(GenesisForkSchedule::parse(json).is_err(), "{json}");
        }
    }
}
