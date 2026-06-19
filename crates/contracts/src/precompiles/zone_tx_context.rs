//! `ZoneTxContext` — Zone L2 precompile.

crate::sol! {
    #[derive(Debug)]
    contract ZoneTxContext {
        function currentTxHash() external returns (bytes32);
    }
}
