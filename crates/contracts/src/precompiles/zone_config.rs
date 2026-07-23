//! `ZoneConfig` — Zone L2 system contract (0x1c00...0003).

crate::sol! {
    #[derive(Debug)]
    contract ZoneConfig {
        function isAccessEnforced() external view returns (bool);
        function isGatewayOpen() external view returns (bool);
        function isAllowedAccount(address account) external view returns (bool);
        function isZoneGateway(address gateway) external view returns (bool);
        function isEnabledToken(address token) external view returns (bool);
    }
}
