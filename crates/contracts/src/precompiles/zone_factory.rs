//! `ZoneFactory` — deployed on Tempo L1.

pub use ZoneFactory::ZoneInfo;

crate::sol! {
    #[derive(Debug)]
    contract ZoneFactory {
        struct ZoneInfo {
            uint32 zoneId;
            address portal;
            address messenger;
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
            address admin;
            address sequencer;
            address verifier;
            ZoneParams zoneParams;
            string rpcUrl;
        }
        event ZoneCreated(
            uint32 indexed zoneId,
            address indexed portal,
            address indexed messenger,
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
        function zones(uint32 zoneId) external view returns (ZoneInfo memory);
        function zoneCount() external view returns (uint32);
        function isZonePortal(address portal) external view returns (bool);
        function isZoneMessenger(address messenger) external view returns (bool);
    }
}
