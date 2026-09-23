//! Inspectable arguments and JSON-RPC requests for the native Nitro verifier.

use alloy_eips::BlockId;
use alloy_json_rpc::{Id, Request};
use alloy_network::Ethereum;
use alloy_primitives::{Address, Bytes};
use alloy_provider::EthCallParams;
use alloy_rpc_types_eth::{TransactionInput, TransactionRequest};
use alloy_sol_types::{SolCall, sol};
use eyre::{Context, Result, bail};
use serde_json::{Value, json};
use tempo_precompiles::zone_factory::portal_address;
use tempo_zone_contracts::ZONE_VERIFIER_ADDRESS;

// Keep the verifier wire ABI independent of the local STF schema, just like `prove`.
sol! {
    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct BlockTransition {
        bytes32 prevBlockHash;
        bytes32 nextBlockHash;
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct DepositQueueTransition {
        bytes32 prevProcessedHash;
        bytes32 nextProcessedHash;
        uint64 prevDepositNumber;
        uint64 nextDepositNumber;
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    struct TokenEnablementTransition {
        uint64 prevProcessedTokenCount;
        uint64 nextProcessedTokenCount;
    }

    #[derive(Debug, serde::Serialize, serde::Deserialize)]
    function verify(
        uint32 zoneId,
        uint64 tempoBlockNumber,
        uint64 anchorBlockNumber,
        bytes32 anchorBlockHash,
        uint64 expectedWithdrawalBatchIndex,
        uint256 nextZoneHeight,
        BlockTransition blockTransition,
        DepositQueueTransition depositQueueTransition,
        TokenEnablementTransition tokenEnablementTransition,
        bytes32 withdrawalQueueHash,
        bytes verifierConfig,
        bytes proof
    ) external view returns (bool);
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct VerifierRequest {
    pub chain_id: u64,
    pub arguments: verifyCall,
    pub rpc: Request<EthCallParams<Ethereum>>,
}

pub(super) fn build(witness: &Value, response: &Value) -> Result<VerifierRequest> {
    let inputs = &witness["publicInputs"];
    let output = &response["output"];
    let chain_id: u64 = serde_json::from_value(inputs["parentChainId"].clone())
        .context("invalid or missing publicInputs.parentChainId")?;
    let call: verifyCall = serde_json::from_value(json!({
        "zoneId": inputs["zoneId"],
        "tempoBlockNumber": inputs["tempoBlockNumber"],
        "anchorBlockNumber": inputs["anchorBlockNumber"],
        "anchorBlockHash": inputs["anchorBlockHash"],
        "expectedWithdrawalBatchIndex": inputs["expectedWithdrawalBatchIndex"],
        "nextZoneHeight": output["nextZoneHeight"],
        "blockTransition": output["blockTransition"],
        "depositQueueTransition": output["depositQueueTransition"],
        "tokenEnablementTransition": output["tokenEnablementTransition"],
        "withdrawalQueueHash": output["withdrawalQueueHash"],
        "verifierConfig": response["proofBundle"]["verifierConfig"],
        "proof": response["proofBundle"]["proof"],
    }))
    .context("invalid or missing native verifier arguments")?;

    let portal = portal_address(call.zoneId);
    // Older witnesses carried an explicit portal. Do not silently change its domain.
    if let Some(value) = inputs.get("portal") {
        let supplied: Address =
            serde_json::from_value(value.clone()).context("invalid witness portal")?;
        if supplied != portal {
            bail!("witness portal {supplied} does not match canonical Zone portal {portal}");
        }
    }
    let data = Bytes::from(call.abi_encode());
    let tx = TransactionRequest {
        from: Some(portal),
        to: Some(ZONE_VERIFIER_ADDRESS.into()),
        // Keep the existing `data` spelling and omit all unspecified fields.
        input: TransactionInput {
            input: None,
            data: Some(data),
        },
        gas: Some(30_000_000),
        ..Default::default()
    };
    let params = EthCallParams::<Ethereum>::new(tx).with_block(BlockId::latest());
    Ok(VerifierRequest {
        chain_id,
        arguments: call,
        rpc: Request::new("eth_call", Id::Number(1), params),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, U256, address};

    fn fixture() -> (Value, Value) {
        (
            json!({"publicInputs": {
                "parentChainId": 31319, "zoneId": 2, "tempoBlockNumber": 3,
                "anchorBlockNumber": 4, "anchorBlockHash": B256::repeat_byte(5),
                "expectedWithdrawalBatchIndex": 6,
            }}),
            json!({
                "output": {
                    "nextZoneHeight": 927,
                    "blockTransition": { "prevBlockHash": B256::repeat_byte(7), "nextBlockHash": B256::repeat_byte(8) },
                    "depositQueueTransition": {
                        "prevProcessedHash": B256::repeat_byte(9), "nextProcessedHash": B256::repeat_byte(10),
                        "prevDepositNumber": 11, "nextDepositNumber": 12,
                    },
                    "tokenEnablementTransition": { "prevProcessedTokenCount": 13, "nextProcessedTokenCount": 14 },
                    "withdrawalQueueHash": B256::repeat_byte(15),
                },
                "proofBundle": { "verifierConfig": "0x01", "proof": "0x1234" },
            }),
        )
    }

    #[test]
    fn builds_inspectable_arguments_and_matching_calldata() {
        let (witness, response) = fixture();
        let typed_request = build(&witness, &response).unwrap();
        let request = serde_json::to_value(&typed_request).unwrap();
        assert_eq!(request["chainId"], 31319);
        assert_eq!(request["arguments"].as_object().unwrap().len(), 12);
        let tx = &request["rpc"]["params"][0];
        assert_eq!(
            tx["from"],
            json!(address!("5ad0000000000000000000000000000000000002"))
        );
        assert_eq!(tx["to"], json!(ZONE_VERIFIER_ADDRESS));
        assert_eq!(request["rpc"]["method"], "eth_call");
        assert_eq!(request["rpc"]["params"][1], "latest");
        let data: Bytes = serde_json::from_value(tx["data"].clone()).unwrap();
        // Compare the entire envelope, including omissions, to the previous wire shape.
        assert_eq!(
            request,
            json!({
                "chainId": 31319,
                "arguments": typed_request.arguments,
                "rpc": {
                    "jsonrpc": "2.0", "id": 1, "method": "eth_call",
                    "params": [{
                        "from": address!("5ad0000000000000000000000000000000000002"),
                        "to": ZONE_VERIFIER_ADDRESS,
                        "data": data,
                        "gas": "0x1c9c380",
                    }, "latest"],
                },
            })
        );
        // IZoneVerifier.verify includes nextZoneHeight after expectedWithdrawalBatchIndex.
        assert_eq!(&data[..4], &[0xeb, 0xb2, 0xdd, 0xc9]);
        let decoded = verifyCall::abi_decode(&data).unwrap();
        assert_eq!(
            serde_json::to_value(&decoded).unwrap(),
            request["arguments"]
        );
        assert_eq!(decoded.zoneId, 2);
        assert_eq!(decoded.tempoBlockNumber, 3);
        assert_eq!(decoded.anchorBlockNumber, 4);
        assert_eq!(decoded.anchorBlockHash, B256::repeat_byte(5));
        assert_eq!(decoded.expectedWithdrawalBatchIndex, 6);
        assert_eq!(decoded.nextZoneHeight, U256::from(927));
        assert_eq!(decoded.blockTransition.nextBlockHash, B256::repeat_byte(8));
        assert_eq!(decoded.depositQueueTransition.nextDepositNumber, 12);
        assert_eq!(
            decoded.tokenEnablementTransition.nextProcessedTokenCount,
            14
        );
        assert_eq!(decoded.withdrawalQueueHash, B256::repeat_byte(15));
        assert_eq!(decoded.proof, Bytes::from_static(&[0x12, 0x34]));
    }

    #[test]
    fn rejects_missing_next_zone_height() {
        let (witness, mut response) = fixture();
        response["output"]
            .as_object_mut()
            .unwrap()
            .remove("nextZoneHeight");
        assert!(build(&witness, &response).is_err());
    }

    #[test]
    fn rejects_incomplete_arguments_and_mismatched_portal() {
        let (mut witness, mut response) = fixture();
        witness["publicInputs"]["portal"] = json!(Address::ZERO);
        assert!(
            build(&witness, &response)
                .unwrap_err()
                .to_string()
                .contains("portal")
        );
        witness["publicInputs"]
            .as_object_mut()
            .unwrap()
            .remove("portal");
        response["output"]
            .as_object_mut()
            .unwrap()
            .remove("tokenEnablementTransition");
        assert!(build(&witness, &response).is_err());
    }
}
