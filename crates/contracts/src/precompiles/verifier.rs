crate::sol! {
    /// Enshrined verifier interface for zone proof/attestation verification.
    ///
    /// This precompile verifies batch proofs submitted to zone portals.
    /// For prototyping, this stub always returns true.
    #[derive(Debug, PartialEq, Eq)]
    #[sol(abi)]
    #[allow(clippy::too_many_arguments)]
    interface IVerifier {
        struct BlockTransition {
            bytes32 prevBlockHash;
            bytes32 nextBlockHash;
        }

        struct DepositQueueTransition {
            bytes32 prevProcessedHash;
            bytes32 nextProcessedHash;
        }

        struct WithdrawalQueueTransition {
            bytes32 withdrawalQueueHash;
        }

        /// Verify a batch proof
        /// @param tempoBlockNumber Block zone committed to (from TempoState)
        /// @param anchorBlockNumber Block whose hash is verified (tempoBlockNumber or recent block)
        /// @param anchorBlockHash Hash of anchorBlockNumber (from EIP-2935)
        /// @param expectedWithdrawalBatchIndex Expected batch index (portal.withdrawalBatchIndex + 1)
        /// @param sequencer Sequencer address (zone block beneficiary must match)
        /// @param blockTransition Zone block hash transition
        /// @param depositQueueTransition Deposit queue processing transition
        /// @param withdrawalQueueTransition Withdrawal queue hash for this batch
        /// @param verifierConfig Opaque payload for verifier (TEE attestation envelope, etc.)
        /// @param proof Validity proof or TEE attestation
        function verify(
            uint64 tempoBlockNumber,
            uint64 anchorBlockNumber,
            bytes32 anchorBlockHash,
            uint64 expectedWithdrawalBatchIndex,
            address sequencer,
            BlockTransition calldata blockTransition,
            DepositQueueTransition calldata depositQueueTransition,
            WithdrawalQueueTransition calldata withdrawalQueueTransition,
            bytes calldata verifierConfig,
            bytes calldata proof
        ) external view returns (bool);
    }
}
