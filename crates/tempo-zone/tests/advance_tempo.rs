//! Reproduce the `advanceTempo` system tx reverting at 232 gas.
//!
//! Run with: `cargo test -p zone --test advance_tempo -- --nocapture`

use alloy_evm::Evm;
use alloy_primitives::{Address, Bytes};
use alloy_sol_types::{SolCall, sol};
use revm::context::result::{ExecutionResult, Output};
use zone_test_utils::{TEMPO_STATE_ADDRESS, ZONE_INBOX_ADDRESS};

sol! {
    function advanceTempo(bytes calldata header, QueuedDeposit[] calldata deposits, DecryptionData[] calldata decryptions);
    function config() external view returns (address);
    function tempoBlockHash() external view returns (bytes32);

    struct QueuedDeposit {
        uint8 depositType;
        bytes depositData;
    }
    struct ChaumPedersenProof {
        bytes32 s;
        bytes32 c;
    }
    struct DecryptionData {
        bytes32 sharedSecret;
        uint8 sharedSecretYParity;
        address to;
        bytes32 memo;
        ChaumPedersenProof cpProof;
    }
}

#[test]
fn advance_tempo_repro() {
    let mut evm = zone_test_utils::setup_zone_evm_default();
    zone_test_utils::register_mock_tempo_state_reader(&mut evm);

    // System txs use Address::ZERO which bypasses OnlySequencer in ZoneInbox/ZoneOutbox
    let sequencer = Address::ZERO;

    // ---------------------------------------------------------------
    // Step 1: Call config() on ZoneInbox — simple view call to verify contracts work
    // ---------------------------------------------------------------
    println!("\n=== Calling ZoneInbox.config() ===");
    let config_calldata = configCall {}.abi_encode();
    let config_result =
        evm.transact_system_call(sequencer, ZONE_INBOX_ADDRESS, Bytes::from(config_calldata));
    match &config_result {
        Ok(result) => {
            println!("config() result: {:?}", result.result);
            match &result.result {
                ExecutionResult::Success {
                    output, gas_used, ..
                } => {
                    if let Output::Call(data) = output {
                        println!("config() returned: {}", data);
                    }
                    println!("config() gas_used: {gas_used}");
                }
                ExecutionResult::Revert {
                    output, gas_used, ..
                } => {
                    println!("config() REVERTED: {output}, gas_used: {gas_used}");
                }
                ExecutionResult::Halt {
                    reason, gas_used, ..
                } => {
                    println!("config() HALTED: {reason:?}, gas_used: {gas_used}");
                }
            }
        }
        Err(e) => println!("config() ERROR: {e:?}"),
    }

    // ---------------------------------------------------------------
    // Step 2: Call tempoBlockHash() on TempoState to verify it works
    // ---------------------------------------------------------------
    println!("\n=== Calling TempoState.tempoBlockHash() ===");
    let hash_calldata = tempoBlockHashCall {}.abi_encode();
    let hash_result =
        evm.transact_system_call(sequencer, TEMPO_STATE_ADDRESS, Bytes::from(hash_calldata));
    match &hash_result {
        Ok(result) => match &result.result {
            ExecutionResult::Success {
                output, gas_used, ..
            } => {
                if let Output::Call(data) = output {
                    println!("tempoBlockHash() returned: {}", data);
                }
                println!("tempoBlockHash() gas_used: {gas_used}");
            }
            ExecutionResult::Revert {
                output, gas_used, ..
            } => {
                println!("tempoBlockHash() REVERTED: {output}, gas_used: {gas_used}");
            }
            other => println!("tempoBlockHash() other: {other:?}"),
        },
        Err(e) => println!("tempoBlockHash() ERROR: {e:?}"),
    }

    // ---------------------------------------------------------------
    // Step 3: Call advanceTempo with a minimal next header
    // ---------------------------------------------------------------
    // Verify contracts have code
    {
        let inbox_code = evm
            .db_mut()
            .cache
            .accounts
            .get(&ZONE_INBOX_ADDRESS)
            .and_then(|a| a.info.code.as_ref())
            .map(|c| c.len())
            .unwrap_or(0);
        let tempostate_code = evm
            .db_mut()
            .cache
            .accounts
            .get(&TEMPO_STATE_ADDRESS)
            .and_then(|a| a.info.code.as_ref())
            .map(|c| c.len())
            .unwrap_or(0);
        println!("ZoneInbox code size: {inbox_code}");
        println!("TempoState code size: {tempostate_code}");
    }

    // ---------------------------------------------------------------
    // Step 2.5: Call finalizeTempo directly on TempoState to isolate
    // ---------------------------------------------------------------
    println!("\n=== Building child header ===");

    // Build a "next" header that's a child of the genesis.
    // finalizeTempo requires: tempoParentHash == prev tempoBlockHash, tempoBlockNumber == prev + 1
    let genesis_hash = {
        use alloy_rlp::Encodable;
        let genesis = tempo_primitives::TempoHeader::default();
        let mut buf = Vec::new();
        genesis.encode(&mut buf);
        alloy_primitives::keccak256(&buf)
    };
    println!("Genesis hash (computed): {genesis_hash}");

    let next_header = tempo_primitives::TempoHeader {
        inner: alloy_consensus::Header {
            number: 1,
            parent_hash: genesis_hash,
            ..Default::default()
        },
        ..Default::default()
    };
    let next_header_rlp = {
        use alloy_rlp::Encodable;
        let mut buf = Vec::new();
        next_header.encode(&mut buf);
        buf
    };
    println!("Next header RLP length: {}", next_header_rlp.len());
    println!("Next header block number: {}", next_header.inner.number);
    println!("Next header parent hash: {}", next_header.inner.parent_hash);

    // NOTE: finalizeTempo() tested separately and works (86729 gas).
    // Skip calling it directly here to avoid corrupting state for the advanceTempo call.

    println!("\n=== Calling ZoneInbox.advanceTempo() ===");

    let advance_calldata = advanceTempoCall {
        header: Bytes::from(next_header_rlp),
        deposits: vec![],
        decryptions: vec![],
    }
    .abi_encode();

    println!(
        "advanceTempo calldata length: {} bytes",
        advance_calldata.len()
    );
    println!(
        "advanceTempo selector: 0x{}",
        const_hex::encode(&advance_calldata[..4])
    );

    let advance_result = evm.transact_system_call(
        sequencer,
        ZONE_INBOX_ADDRESS,
        Bytes::from(advance_calldata.clone()),
    );
    match &advance_result {
        Ok(result) => match &result.result {
            ExecutionResult::Success {
                output, gas_used, ..
            } => {
                if let Output::Call(data) = output {
                    println!("advanceTempo() SUCCESS, output: {}", data);
                }
                println!("advanceTempo() gas_used: {gas_used}");
            }
            ExecutionResult::Revert {
                output, gas_used, ..
            } => {
                println!("advanceTempo() REVERTED: {output}");
                println!("advanceTempo() gas_used: {gas_used}");
                if output.len() >= 4 {
                    let sel = &output[..4];
                    println!("  error selector: 0x{}", const_hex::encode(sel));
                    if sel == [0x08, 0xc3, 0x79, 0xa0] && output.len() > 4 {
                        if let Ok(msg) = <alloy_sol_types::sol_data::String as alloy_sol_types::SolType>::abi_decode(&output[4..]) {
                            println!("  Error message: {msg}");
                        }
                    }
                }
            }
            ExecutionResult::Halt {
                reason, gas_used, ..
            } => {
                println!("advanceTempo() HALTED: {reason:?}");
                println!("advanceTempo() gas_used: {gas_used}");
            }
        },
        Err(e) => println!("advanceTempo() ERROR: {e:?}"),
    }

    // With the mock precompile registered, advanceTempo should now succeed.
    let result = advance_result.expect("advanceTempo tx should not error");
    assert!(
        result.result.is_success(),
        "advanceTempo should succeed with mock precompile, got: {:?}",
        result.result,
    );
    println!("\n=== Test complete ===");
}
