//! `ZoneConfig` — Zone L2 system contract (0x1c00...0003).

crate::sol! {
    #[derive(Debug)]
    contract ZoneConfig {
        function accessMode() external view returns (uint8);
        function isAllowedAccount(address account) external view returns (bool);
        function isZoneGateway(address gateway) external view returns (bool);
        function isEnabledToken(address token) external view returns (bool);
    }
}
