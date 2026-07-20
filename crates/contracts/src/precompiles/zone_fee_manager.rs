//! `ZoneFeeManager` — Zone L2 protocol fee manager.

crate::sol! {
    #[derive(Debug)]
    contract IZoneFeeManager {
        event FeesDistributed(address indexed sequencer, address indexed token, uint256 amount);

        function collectedFees(address sequencer, address token) external view returns (uint256);
        function distributeFees(address sequencer, address token) external;
        function isEnabledToken(address token) external view returns (bool);
    }
}
