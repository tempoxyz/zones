//! CLI wrapper for the reusable Portal backing verifier.

use alloy::{primitives::Address, providers::ProviderBuilder};
use eyre::WrapErr as _;
use tempo_alloy::TempoNetwork;
use zone_portal_backing::{PortalBackingReport, PortalBackingRequest, audit_portal_backing};

use crate::zone_utils::normalize_http_rpc;

#[derive(Debug, clap::Parser)]
pub(crate) struct VerifyPortalBacking {
    /// Tempo L1 HTTP RPC URL.
    #[arg(long, env = "L1_RPC_URL")]
    l1_rpc_url: String,

    /// Full operator Zone HTTP RPC URL.
    #[arg(long, env = "ZONE_RPC_URL")]
    zone_rpc_url: String,

    /// ZonePortal address on Tempo L1.
    #[arg(long)]
    portal: Address,

    /// TIP-20 token address, shared by Tempo L1 and the Zone.
    #[arg(long)]
    token: Address,

    /// First L1 block to scan. Must include the Portal's complete event history.
    #[arg(long, default_value_t = 0)]
    l1_from_block: u64,

    /// First Zone block to scan. Must include the ZoneInbox's complete event history.
    #[arg(long, default_value_t = 0)]
    zone_from_block: u64,
}

impl VerifyPortalBacking {
    pub(crate) async fn run(self) -> eyre::Result<()> {
        let l1_rpc_url = normalize_http_rpc(&self.l1_rpc_url);
        let zone_rpc_url = normalize_http_rpc(&self.zone_rpc_url);
        let l1 = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&l1_rpc_url)
            .await
            .wrap_err("failed connecting to Tempo L1")?;
        let zone = ProviderBuilder::new_with_network::<TempoNetwork>()
            .connect(&zone_rpc_url)
            .await
            .wrap_err("failed connecting to Zone RPC")?;
        let report = audit_portal_backing(
            &l1,
            &zone,
            PortalBackingRequest {
                portal: self.portal,
                token: self.token,
                l1_from_block: self.l1_from_block,
                zone_from_block: self.zone_from_block,
            },
        )
        .await?;

        print_report(&report);
        report.ensure_solvent()
    }
}

fn print_report(report: &PortalBackingReport) {
    println!("Portal backing audit");
    println!("  L1 snapshot block:       {}", report.l1_snapshot_block);
    println!("  L1 snapshot hash:        {}", report.l1_snapshot_hash);
    println!("  Zone snapshot block:     {}", report.zone_snapshot_block);
    println!("  Zone snapshot hash:      {}", report.zone_snapshot_hash);
    println!(
        "  L1 event scan:           {}..={}",
        report.l1_from_block, report.l1_snapshot_block
    );
    println!(
        "  Zone event scan:         {}..={}",
        report.zone_from_block, report.zone_snapshot_block
    );
    println!("  Portal:                  {}", report.portal);
    println!("  Token:                   {}", report.token);
    println!("  Portal balance:          {}", report.portal_balance);
    println!("  Zone total supply:       {}", report.zone_total_supply);
    println!(
        "  Deposit queue:           portal={}, l1-settled={}, zone={}",
        report.deposit_count, report.l1_processed_deposits, report.zone_processed_deposits
    );
    println!(
        "  Withdrawal queue:        head={}, tail={}",
        report.withdrawal_queue_head, report.withdrawal_queue_tail
    );
    println!(
        "  Pending deposit liability: {}",
        report.pending_deposit_liability
    );
    println!(
        "  Pending withdrawal liability: {}",
        report.pending_withdrawal_liability
    );
    println!(
        "  Portal refund liability: {}",
        report.portal_refund_liability
    );
    println!(
        "  Inbox refund liability:  {}",
        report.inbox_refund_liability
    );
    println!("  Required backing:        {}", report.required_backing);

    if report.is_solvent() {
        println!("  PASS: backing surplus    {}", report.backing_surplus);
    } else {
        println!("  FAIL: backing deficit    {}", report.backing_deficit);
    }
}
