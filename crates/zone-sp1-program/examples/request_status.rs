use std::{error::Error, str::FromStr};

use sp1_sdk::{
    ProverClient,
    network::{
        B256,
        proto::types::{
            ExecuteFailureCause, ExecutionStatus, FulfillmentStatus, ProofRequestError,
            SettlementStatus,
        },
    },
};

fn fmt_fulfillment_status(raw: i32) -> String {
    FulfillmentStatus::try_from(raw)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| format!("UNKNOWN({raw})"))
}

fn fmt_execution_status(raw: i32) -> String {
    ExecutionStatus::try_from(raw)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| format!("UNKNOWN({raw})"))
}

fn fmt_execute_fail_cause(raw: i32) -> String {
    ExecuteFailureCause::try_from(raw)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| format!("UNKNOWN({raw})"))
}

fn fmt_proof_request_error(raw: i32) -> String {
    ProofRequestError::try_from(raw)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| format!("UNKNOWN({raw})"))
}

fn fmt_settlement_status(raw: i32) -> String {
    SettlementStatus::try_from(raw)
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|_| format!("UNKNOWN({raw})"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let request_id_hex = std::env::args()
        .nth(1)
        .ok_or("usage: request_status <0x...request_id>")?;
    let request_id = B256::from_str(&request_id_hex)?;

    let prover = ProverClient::builder().network().build().await;
    let request = prover.get_proof_request(request_id).await?;
    let (status, maybe_proof) = prover.get_proof_status(request_id).await?;

    println!("request_id={request_id}");
    println!(
        "fulfillment_status={} ({})",
        status.fulfillment_status(),
        fmt_fulfillment_status(status.fulfillment_status()),
    );
    println!(
        "execution_status={} ({})",
        status.execution_status(),
        fmt_execution_status(status.execution_status()),
    );
    println!("deadline_unix={}", status.deadline());
    println!("proof_available={}", maybe_proof.is_some());

    if let Some(req) = request {
        println!("created_at_unix={}", req.created_at);
        println!("updated_at_unix={}", req.updated_at);
        println!("cycle_limit={}", req.cycle_limit);
        println!("gas_limit={}", req.gas_limit);
        println!("cycles={:?}", req.cycles);
        println!("gas_used={:?}", req.gas_used);
        println!("gas_price={:?}", req.gas_price);
        println!("deduction_amount={:?}", req.deduction_amount);
        println!("refund_amount={:?}", req.refund_amount);
        println!(
            "execute_fail_cause={} ({})",
            req.execute_fail_cause,
            fmt_execute_fail_cause(req.execute_fail_cause),
        );
        println!(
            "error={} ({})",
            req.error,
            fmt_proof_request_error(req.error)
        );
        println!(
            "settlement_status={} ({})",
            req.settlement_status,
            fmt_settlement_status(req.settlement_status),
        );
    } else {
        println!("request_details=none");
    }

    Ok(())
}
