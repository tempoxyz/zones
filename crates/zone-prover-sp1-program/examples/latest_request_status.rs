use std::error::Error;

use sp1_sdk::ProverClient;
use sp1_sdk::network::{
    client::NetworkClient,
    proto::{
        GetFilteredProofRequestsResponse,
        types::{ExecutionStatus, FulfillmentStatus},
    },
    signer::NetworkSigner,
    Address, B256, NetworkMode,
};

fn network_mode_from_env() -> NetworkMode {
    match std::env::var("NETWORK_RPC_URL") {
        Ok(url) if url.contains("production.succinct.xyz") || url.contains("reserved") => {
            NetworkMode::Reserved
        }
        _ => NetworkMode::Mainnet,
    }
}

fn explorer_base_from_env(mode: NetworkMode) -> &'static str {
    match std::env::var("NETWORK_RPC_URL") {
        Ok(url) => match url.trim_end_matches('/') {
            "https://rpc.private.succinct.xyz" => "https://explorer-private.succinct.xyz",
            "https://rpc.production.succinct.xyz" => "https://explorer.reserved.succinct.xyz",
            "https://rpc.mainnet.succinct.xyz" => "https://explorer.succinct.xyz",
            _ => match mode {
                NetworkMode::Mainnet => "https://explorer.succinct.xyz",
                NetworkMode::Reserved => "https://explorer.reserved.succinct.xyz",
            },
        },
        Err(_) => match mode {
            NetworkMode::Mainnet => "https://explorer.succinct.xyz",
            NetworkMode::Reserved => "https://explorer.reserved.succinct.xyz",
        },
    }
}

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let private_key = std::env::var("NETWORK_PRIVATE_KEY")
        .map_err(|_| "NETWORK_PRIVATE_KEY must be set (source .env first)")?;
    let signer = NetworkSigner::local(&private_key)?;
    let requester = signer.address();

    let mode = network_mode_from_env();
    let rpc_url = std::env::var("NETWORK_RPC_URL").unwrap_or_else(|_| match mode {
        NetworkMode::Mainnet => "https://rpc.mainnet.succinct.xyz".to_string(),
        NetworkMode::Reserved => "https://rpc.production.succinct.xyz".to_string(),
    });
    let explorer = explorer_base_from_env(mode);

    // Installs rustls CryptoProvider the same way the normal SP1 network prover does.
    let _ = ProverClient::builder().network().build().await;

    let client = NetworkClient::new(signer, rpc_url, mode);

    match mode {
        NetworkMode::Mainnet => {
            let mut all_requests = Vec::new();
            for page in 1..=10 {
                let response = client
                    .get_filtered_proof_requests(
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(requester.to_vec()),
                        None,
                        None,
                        None,
                        Some(100),
                        Some(page),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await?;
                if let GetFilteredProofRequestsResponse::Auction(resp) = response {
                    if resp.requests.is_empty() {
                        break;
                    }
                    all_requests.extend(resp.requests);
                }
            }

            if all_requests.is_empty() {
                return Err("no proof requests found for this account".into());
            }

            all_requests.sort_by_key(|r| (r.created_at, r.updated_at));
            let latest = all_requests.last().expect("checked non-empty");
            let request_id = B256::from_slice(&latest.request_id);
            let requester_addr = Address::from_slice(&latest.requester);

            println!("request_id={request_id}");
            println!("explorer_url={explorer}/request/{request_id}");
            println!("requester={requester_addr}");
            println!("version={}", latest.version);
            println!("fulfillment_status={}", fmt_fulfillment_status(latest.fulfillment_status));
            println!("execution_status={}", fmt_execution_status(latest.execution_status));
            println!("created_at_unix={}", latest.created_at);
            println!("updated_at_unix={}", latest.updated_at);
            println!("cycles={:?}", latest.cycles);
            println!("gas_used={:?}", latest.gas_used);
            println!("gas_price={:?}", latest.gas_price);
            println!("deduction_amount={:?}", latest.deduction_amount);
            println!("refund_amount={:?}", latest.refund_amount);
        }
        NetworkMode::Reserved => {
            let mut all_requests = Vec::new();
            for page in 1..=10 {
                let response = client
                    .get_filtered_proof_requests(
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(requester.to_vec()),
                        None,
                        None,
                        None,
                        Some(100),
                        Some(page),
                        None,
                        None,
                        None,
                        None,
                        None,
                    )
                    .await?;
                if let GetFilteredProofRequestsResponse::Base(resp) = response {
                    if resp.requests.is_empty() {
                        break;
                    }
                    all_requests.extend(resp.requests);
                }
            }

            if all_requests.is_empty() {
                return Err("no proof requests found for this account".into());
            }
            all_requests.sort_by_key(|r| (r.created_at, r.updated_at));
            let latest = all_requests.last().expect("checked non-empty");
            let request_id = B256::from_slice(&latest.request_id);
            println!("request_id={request_id}");
            println!("explorer_url={explorer}/request/{request_id}");
            println!("fulfillment_status_raw={}", latest.fulfillment_status);
            println!("execution_status_raw={}", latest.execution_status);
            println!("created_at_unix={}", latest.created_at);
            println!("updated_at_unix={}", latest.updated_at);
        }
    }

    Ok(())
}
