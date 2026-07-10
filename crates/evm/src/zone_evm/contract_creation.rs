//! Contract creation validation and runtime enforcement.

use alloy_evm::Database;
use alloy_primitives::Address;
use revm::{
    bytecode::opcode::{CREATE, CREATE2},
    context::Transaction,
    interpreter::{
        Instruction, InstructionContext, InstructionResult,
        instructions::contract::create as revm_create, interpreter::EthInterpreter,
        interpreter_types::InputsTr,
    },
};
use tempo_evm::evm::TempoEvm;
use tempo_revm::{TempoInvalidTransaction, TempoTxEnv, evm::TempoContext};
use zone_primitives::constants::CONTRACT_DEPLOYER_ALLOWLIST;

type ZoneInstructionCtx<'a, DB> = InstructionContext<'a, TempoContext<DB>, EthInterpreter>;

/// Installs the Zone contract-creation policy into the EVM instruction table.
///
/// The standard `CREATE` and `CREATE2` handlers are replaced with guarded variants that check the
/// executing storage-context address against [`CONTRACT_DEPLOYER_ALLOWLIST`]. Allowed deployers
/// delegate to revm's standard instruction implementation, preserving its gas accounting and
/// execution semantics; unlisted deployers halt with [`InstructionResult::NotActivated`].
pub(super) fn configure_runtime<DB: Database, I>(evm: &mut TempoEvm<DB, I>) {
    let instructions = &mut evm.inner_mut().inner.instruction;
    instructions.insert_instruction(
        CREATE,
        Instruction::new(|ctx| create::<false, DB>(ctx, CONTRACT_DEPLOYER_ALLOWLIST)),
        0,
    );
    instructions.insert_instruction(
        CREATE2,
        Instruction::new(|ctx| create::<true, DB>(ctx, CONTRACT_DEPLOYER_ALLOWLIST)),
        0,
    );
}

fn create<const IS_CREATE2: bool, DB: Database>(
    context: ZoneInstructionCtx<'_, DB>,
    allowlist: &[Address],
) -> Result<(), InstructionResult> {
    if !allowlist.contains(&context.interpreter.input.target_address()) {
        return Err(InstructionResult::NotActivated);
    }

    revm_create::<IS_CREATE2, EthInterpreter, TempoContext<DB>>(context)
}

/// Reject transaction-level code creation unless its deployer is explicitly allowed.
pub(super) fn validate_transaction(
    tx: &TempoTxEnv,
    allowlist: &[Address],
) -> Result<(), TempoInvalidTransaction> {
    if contract_creation_deployer(tx).is_some_and(|deployer| !allowlist.contains(&deployer)) {
        return Err(TempoInvalidTransaction::CallsValidation(
            "contract creation is not supported",
        ));
    }

    let has_code_authorization = tx.authorization_list_len() != 0
        || tx
            .tempo_tx_env
            .as_ref()
            .is_some_and(|aa| !aa.tempo_authorization_list.is_empty());

    if has_code_authorization {
        return Err(TempoInvalidTransaction::CallsValidation(
            "code authorization is not supported",
        ));
    }

    Ok(())
}

fn contract_creation_deployer(tx: &TempoTxEnv) -> Option<Address> {
    let creates = match tx.tempo_tx_env.as_ref() {
        Some(aa) => aa.aa_calls.iter().any(|call| call.to.is_create()),
        None => tx.kind().is_create(),
    };
    creates.then_some(tx.caller)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ZoneEvm;
    use alloy_evm::{Evm, EvmEnv};
    use alloy_primitives::{Address, Bytes, Signature, TxKind, U256, bytes};
    use revm::{
        bytecode::Bytecode,
        context::{
            TxEnv,
            either::Either,
            result::{EVMError, ExecutionResult},
            transaction::{Authorization, SignedAuthorization},
        },
        database::{EmptyDB, in_memory_db::CacheDB},
        inspector::NoOpInspector,
        state::AccountInfo,
    };
    use tempo_evm::{TempoBlockEnv, TempoHaltReason};
    use tempo_primitives::transaction::{
        Call, PrimitiveSignature, RecoveredTempoAuthorization, TempoSignature,
        TempoSignedAuthorization,
    };
    use tempo_revm::{TempoBatchCallEnv, TempoTxEnv};

    type TestDb = CacheDB<EmptyDB>;

    const TEST_DEPLOYER: Address = Address::new([0x42; 20]);

    fn test_create<const IS_CREATE2: bool>(
        context: ZoneInstructionCtx<'_, TestDb>,
    ) -> Result<(), InstructionResult> {
        create::<IS_CREATE2, TestDb>(context, &[TEST_DEPLOYER])
    }

    fn enable_test_deployer<I>(evm: &mut ZoneEvm<TestDb, I>) {
        let instructions = &mut evm.inner.inner_mut().inner.instruction;
        instructions.insert_instruction(CREATE, Instruction::new(test_create::<false>), 0);
        instructions.insert_instruction(CREATE2, Instruction::new(test_create::<true>), 0);
    }

    fn test_db(contracts: impl IntoIterator<Item = (Address, Bytes)>) -> TestDb {
        let mut db = CacheDB::new(EmptyDB::default());
        for (address, code) in contracts {
            db.insert_account_info(
                address,
                AccountInfo {
                    code_hash: alloy_primitives::keccak256(&code),
                    code: Some(Bytecode::new_raw(code)),
                    nonce: 1,
                    ..Default::default()
                },
            );
        }
        db
    }

    fn evm_with_contract(addr: Address, code: &[u8]) -> ZoneEvm<TestDb, NoOpInspector> {
        let input: EvmEnv<tempo_chainspec::hardfork::TempoHardfork, TempoBlockEnv> =
            EvmEnv::default();
        ZoneEvm::new(TempoEvm::new(
            test_db([(addr, Bytes::copy_from_slice(code))]),
            input,
        ))
    }

    fn call_tx(caller: Address, contract: Address) -> TempoTxEnv {
        TempoTxEnv {
            inner: TxEnv {
                caller,
                gas_price: 0,
                gas_limit: 1_000_000,
                kind: TxKind::Call(contract),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn assert_not_activated(result: ExecutionResult<TempoHaltReason>) {
        assert!(matches!(
            result,
            ExecutionResult::Halt {
                reason: TempoHaltReason::Ethereum(revm::context::result::HaltReason::NotActivated),
                ..
            }
        ));
    }

    #[test]
    fn top_level_create_transaction_is_rejected() {
        let mut evm = evm_with_contract(Address::ZERO, &[]);
        let err = evm
            .transact_raw(TempoTxEnv {
                inner: TxEnv {
                    caller: Address::repeat_byte(0x01),
                    gas_price: 0,
                    gas_limit: 1_000_000,
                    kind: TxKind::Create,
                    data: Bytes::from_static(&[0x00]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .expect_err("top-level create must be rejected");

        assert!(matches!(
            err,
            EVMError::Transaction(TempoInvalidTransaction::CallsValidation(..))
        ));
    }

    #[test]
    fn top_level_create_respects_allowlist() {
        let caller = Address::repeat_byte(0x01);
        let tx = TempoTxEnv {
            inner: TxEnv {
                caller,
                kind: TxKind::Create,
                ..Default::default()
            },
            ..Default::default()
        };

        assert!(validate_transaction(&tx, &[]).is_err());
        assert!(validate_transaction(&tx, &[caller]).is_ok());
    }

    #[test]
    fn runtime_create_opcodes_are_disabled_for_unlisted_deployers() {
        let (contract, caller) = (Address::repeat_byte(0x10), Address::repeat_byte(0x20));
        for bytecode in [
            // PUSH0 PUSH0 PUSH0 CREATE STOP
            bytes!("0x5f5f5ff000"),
            // PUSH0 PUSH0 PUSH0 PUSH0 CREATE2 STOP
            bytes!("0x5f5f5f5ff500"),
        ] {
            let mut evm = evm_with_contract(contract, &bytecode);
            let result = evm
                .transact_raw(call_tx(caller, contract))
                .expect("transaction should execute")
                .result;
            assert_not_activated(result);
        }
    }

    #[test]
    fn allowlisted_deployer_can_execute_create_and_create2() {
        let caller = Address::repeat_byte(0x20);
        for bytecode in [
            // CREATE, then return the created address.
            bytes!("0x5f5f5ff05f5260205ff3"),
            // CREATE2, then return the created address.
            bytes!("0x5f5f5f5ff55f5260205ff3"),
        ] {
            let mut evm = evm_with_contract(TEST_DEPLOYER, &bytecode);
            enable_test_deployer(&mut evm);

            let result = evm
                .transact_raw(call_tx(caller, TEST_DEPLOYER))
                .expect("allowlisted deployment should execute");
            let ExecutionResult::Success { output, .. } = result.result else {
                panic!("allowlisted deployment should succeed")
            };
            let created = Address::from_slice(&output.data()[12..]);
            assert_ne!(created, Address::ZERO);
            assert!(result.state.contains_key(&created));
        }
    }

    #[test]
    fn delegatecall_does_not_borrow_deployer_privilege() {
        let caller = Address::repeat_byte(0x20);
        let proxy = Address::repeat_byte(0x30);

        // DELEGATECALL the allowlisted deployer with empty calldata, then return its success flag.
        let mut proxy_code = vec![0x5f, 0x5f, 0x5f, 0x5f, 0x73];
        proxy_code.extend_from_slice(TEST_DEPLOYER.as_slice());
        proxy_code.extend_from_slice(&[0x61, 0xff, 0xff, 0xf4, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);

        let input: EvmEnv<tempo_chainspec::hardfork::TempoHardfork, TempoBlockEnv> =
            EvmEnv::default();
        let mut evm = ZoneEvm::new(TempoEvm::new(
            test_db([
                (proxy, Bytes::from(proxy_code)),
                (TEST_DEPLOYER, bytes!("0x5f5f5ff000")),
            ]),
            input,
        ));
        enable_test_deployer(&mut evm);

        let result = evm
            .transact_raw(call_tx(caller, proxy))
            .expect("proxy call should execute");
        let ExecutionResult::Success { output, .. } = result.result else {
            panic!("proxy should handle the failed delegatecall")
        };
        assert_eq!(output.data()[31], 0);
    }

    #[test]
    fn aa_calls_ignore_synthetic_outer_create_kind() {
        let tx = TempoTxEnv {
            inner: TxEnv {
                caller: Address::repeat_byte(0x11),
                kind: TxKind::Create,
                ..Default::default()
            },
            tempo_tx_env: Some(Box::new(TempoBatchCallEnv {
                aa_calls: vec![Call {
                    to: TxKind::Call(Address::repeat_byte(0x22)),
                    value: U256::ZERO,
                    input: Bytes::new(),
                }],
                ..Default::default()
            })),
            ..Default::default()
        };

        assert!(validate_transaction(&tx, CONTRACT_DEPLOYER_ALLOWLIST).is_ok());
    }

    #[test]
    fn aa_create_respects_allowlist() {
        let caller = Address::repeat_byte(0x11);
        let tx = TempoTxEnv {
            inner: TxEnv {
                caller,
                ..Default::default()
            },
            tempo_tx_env: Some(Box::new(TempoBatchCallEnv {
                aa_calls: vec![Call {
                    to: TxKind::Create,
                    value: U256::ZERO,
                    input: Bytes::new(),
                }],
                ..Default::default()
            })),
            ..Default::default()
        };

        assert!(validate_transaction(&tx, &[]).is_err());
        assert!(validate_transaction(&tx, &[caller]).is_ok());
    }

    #[test]
    fn code_authorizations_are_rejected() {
        let authorization = Authorization {
            chain_id: U256::ONE,
            address: Address::repeat_byte(0x44),
            nonce: 0,
        };
        let signed =
            SignedAuthorization::new_unchecked(authorization.clone(), 0, U256::ONE, U256::ONE);
        let ethereum_tx = TempoTxEnv {
            inner: TxEnv {
                authorization_list: vec![Either::Left(signed)],
                ..Default::default()
            },
            ..Default::default()
        };

        let tempo_authorization = TempoSignedAuthorization::new_unchecked(
            authorization,
            TempoSignature::Primitive(PrimitiveSignature::Secp256k1(Signature::test_signature())),
        );
        let tempo_tx = TempoTxEnv {
            tempo_tx_env: Some(Box::new(TempoBatchCallEnv {
                tempo_authorization_list: vec![RecoveredTempoAuthorization::new(
                    tempo_authorization,
                )],
                ..Default::default()
            })),
            ..Default::default()
        };

        assert!(validate_transaction(&ethereum_tx, CONTRACT_DEPLOYER_ALLOWLIST).is_err());
        assert!(validate_transaction(&tempo_tx, CONTRACT_DEPLOYER_ALLOWLIST).is_err());
    }
}
