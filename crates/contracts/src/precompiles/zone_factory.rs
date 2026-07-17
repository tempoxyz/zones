//! `ZoneFactory` — deployed on Tempo L1.

use alloy_primitives::{Address, address};

pub use ZoneFactory::ZoneInfo;

/// Protocol-managed ZoneFactory address defined by TIP-1091.
pub const ZONE_FACTORY_ADDRESS: Address = address!("0x5aF2000000000000000000000000000000000000");

crate::sol! {
    #[derive(Debug)]
    contract ZoneFactory {
        struct ZoneInfo {
            uint32 zoneId;
            address portal;
            address initialToken;
            address admin;
            address sequencer;
            address verifier;
            bytes32 genesisBlockHash;
            bytes32 genesisTempoBlockHash;
            uint64 genesisTempoBlockNumber;
            string rpcUrl;
        }
        struct ZoneParams {
            bytes32 genesisBlockHash;
            bytes32 genesisTempoBlockHash;
            uint64 genesisTempoBlockNumber;
        }
        struct CreateZoneParams {
            address initialToken;
            address[] allowedAccounts;
            address[] zoneGateways;
            address admin;
            address sequencer;
            address verifier;
            ZoneParams zoneParams;
            string rpcUrl;
        }
        event ZoneCreated(
            uint32 indexed zoneId,
            address indexed portal,
            address initialToken,
            address admin,
            address sequencer,
            address verifier,
            bytes32 genesisBlockHash,
            bytes32 genesisTempoBlockHash,
            uint64 genesisTempoBlockNumber
        );
        function createZone(CreateZoneParams calldata params) external returns (uint32 zoneId, address portal);
        function verifier() external view returns (address);
        function messenger() external view returns (address);
        function zones(uint32 zoneId) external view returns (ZoneInfo memory);
        function zoneCount() external view returns (uint32);
        function isZonePortal(address portal) external view returns (bool);
    }
}
