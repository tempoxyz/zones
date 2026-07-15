//! `ZoneFeeManager` — Zone L2 protocol fee manager.

crate::sol! {
    #[derive(Debug)]
    contract IZoneFeeManager {
        event UserTokenSet(address indexed user, address indexed token);
        event FeesDistributed(address indexed sequencer, address indexed token, uint256 amount);

        function userTokens(address user) external view returns (address);
        function collectedFees(address sequencer, address token) external view returns (uint256);
        function setUserToken(address token) external;
        function distributeFees(address sequencer, address token) external;
        function isEnabledToken(address token) external view returns (bool);
    }
}
