//! Tempo fork activation arguments shared by Zone genesis commands.

use alloy::genesis::ChainConfig;

/// Explicit Tempo fork schedule for newly generated Zone genesis files.
#[derive(Debug, Default, clap::Args)]
pub(crate) struct GenesisForkArgs {
    /// T0 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t0_time: u64,

    /// T1 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t1_time: u64,

    /// T1A activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t1a_time: u64,

    /// T1B activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t1b_time: u64,

    /// T1C activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t1c_time: u64,

    /// T2 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t2_time: u64,

    /// T3 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t3_time: u64,

    /// T4 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t4_time: u64,

    /// T5 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t5_time: u64,

    /// T6 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t6_time: u64,

    /// T7 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t7_time: u64,

    /// T8 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t8_time: u64,

    /// T9 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t9_time: u64,

    /// T10 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t10_time: u64,

    /// T11 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t11_time: u64,

    /// T12 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t12_time: u64,

    /// T13 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t13_time: u64,

    /// T14 activation timestamp (0 = genesis).
    #[arg(long, default_value_t = 0)]
    t14_time: u64,
}

impl GenesisForkArgs {
    pub(crate) fn apply_to(&self, config: &mut ChainConfig) -> eyre::Result<()> {
        for (name, timestamp) in [
            ("t0Time", self.t0_time),
            ("t1Time", self.t1_time),
            ("t1aTime", self.t1a_time),
            ("t1bTime", self.t1b_time),
            ("t1cTime", self.t1c_time),
            ("t2Time", self.t2_time),
            ("t3Time", self.t3_time),
            ("t4Time", self.t4_time),
            ("t5Time", self.t5_time),
            ("t6Time", self.t6_time),
            ("t7Time", self.t7_time),
            ("t8Time", self.t8_time),
            ("t9Time", self.t9_time),
            ("t10Time", self.t10_time),
            ("t11Time", self.t11_time),
            ("t12Time", self.t12_time),
            ("t13Time", self.t13_time),
            ("t14Time", self.t14_time),
        ] {
            config.extra_fields.insert_value(name.into(), timestamp)?;
        }
        Ok(())
    }
}
