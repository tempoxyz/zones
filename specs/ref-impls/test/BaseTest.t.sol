// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import {
    BlockTransition,
    DepositQueueTransition,
    IZoneFactory,
    IZonePortal,
    Withdrawal,
    ZONE_FACTORY_ADDRESS,
    ZONE_MESSENGER_ADDRESS,
    ZONE_TX_CONTEXT,
    ZONE_VERIFIER_ADDRESS,
    ZoneInfo
} from "../src/interfaces/IZone.sol";
import { EIP2935 } from "../src/libraries/BlockHashHistory.sol";
import { Verifier } from "../src/tempo/Verifier.sol";
import { ZoneMessenger } from "../src/tempo/ZoneMessenger.sol";
import { ZonePortal } from "../src/tempo/ZonePortal.sol";
import { MockZoneGateway } from "./mocks/MockZoneGateway.sol";
import { MockZoneTxContext } from "./mocks/MockZoneTxContext.sol";
import { Test, console } from "forge-std/Test.sol";
import { StdPrecompiles } from "tempo-std/StdPrecompiles.sol";
import { IAccountKeychain } from "tempo-std/interfaces/IAccountKeychain.sol";
import { IFeeManager } from "tempo-std/interfaces/IFeeManager.sol";
import { INonce } from "tempo-std/interfaces/INonce.sol";
import { ISignatureVerifier } from "tempo-std/interfaces/ISignatureVerifier.sol";
import { IStablecoinDEX } from "tempo-std/interfaces/IStablecoinDEX.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";
import { ITIP20Token } from "tempo-std/interfaces/ITIP20.sol";
import { ITIP20Factory } from "tempo-std/interfaces/ITIP20Factory.sol";
import { ITIP403Registry } from "tempo-std/interfaces/ITIP403Registry.sol";
import { IValidatorConfig } from "tempo-std/interfaces/IValidatorConfig.sol";

/// @notice Base test framework for all spec tests
/// pathUSD is just a TIP20 at a special address (0x20C0...) with token_id=0
contract BaseTest is Test {

    mapping(address portal => uint256 height) private _submittedZoneHeights;

    // Registry precompiles
    address internal constant _ACCOUNT_KEYCHAIN = StdPrecompiles.ACCOUNT_KEYCHAIN_ADDRESS;
    address internal constant _TIP403REGISTRY = StdPrecompiles.TIP403_REGISTRY_ADDRESS;
    address internal constant _TIP20FACTORY = StdPrecompiles.TIP20_FACTORY_ADDRESS;
    address internal constant _PATH_USD = 0x20C0000000000000000000000000000000000000;
    address internal constant _STABLECOIN_DEX = StdPrecompiles.STABLECOIN_DEX_ADDRESS;
    address internal constant _FEE_AMM = StdPrecompiles.TIP_FEE_MANAGER_ADDRESS;
    address internal constant _NONCE = StdPrecompiles.NONCE_ADDRESS;
    address internal constant _VALIDATOR_CONFIG = StdPrecompiles.VALIDATOR_CONFIG_ADDRESS;
    address internal constant _BLOCKHASH_HISTORY = EIP2935;
    address internal constant _ZONE_TX_CONTEXT = ZONE_TX_CONTEXT;
    address internal constant _ZONE_FACTORY = ZONE_FACTORY_ADDRESS;

    // EIP-2935 serve window: hashes for the most recent 8191 blocks are available
    // (block.number - 8191 ..= block.number - 1); reads outside that range return zero.
    uint256 internal constant BLOCKHASH_HISTORY_WINDOW = 8191;

    // Role constants
    bytes32 internal constant _ISSUER_ROLE = keccak256("ISSUER_ROLE");
    bytes32 internal constant _PAUSE_ROLE = keccak256("PAUSE_ROLE");
    bytes32 internal constant _UNPAUSE_ROLE = keccak256("UNPAUSE_ROLE");
    bytes32 internal constant _TRANSFER_ROLE = keccak256("TRANSFER_ROLE");
    bytes32 internal constant _RECEIVE_WITH_MEMO_ROLE = keccak256("RECEIVE_WITH_MEMO_ROLE");

    // Common test addresses
    address public admin = address(0x500);
    address public sequencer = address(this);
    address public alice = address(0x200);
    address public bob = address(0x300);
    address public charlie = address(0x400);
    address public pathUSDAdmin = address(0xb4c79daB8f259C7Aee6E5b2Aa729821864227e84);

    // Common test contracts
    IAccountKeychain public keychain = IAccountKeychain(_ACCOUNT_KEYCHAIN);
    ITIP20Factory public factory = ITIP20Factory(_TIP20FACTORY);
    ITIP20Token public pathUSD = ITIP20Token(_PATH_USD);
    IStablecoinDEX public exchange = IStablecoinDEX(_STABLECOIN_DEX);
    IFeeManager public amm = IFeeManager(_FEE_AMM);
    ITIP403Registry public registry = ITIP403Registry(_TIP403REGISTRY);
    INonce public nonce = INonce(_NONCE);
    IValidatorConfig public validatorConfig = IValidatorConfig(_VALIDATOR_CONFIG);
    ITIP20Token public token1;
    ITIP20Token public token2;
    MockZoneTxContext public zoneTxContext = MockZoneTxContext(_ZONE_TX_CONTEXT);
    MockZoneGateway public zoneGateway;

    error MissingPrecompile(string name, address addr);
    error CallShouldHaveReverted();

    function setUp() public virtual {
        zoneGateway = new MockZoneGateway();

        if (_ACCOUNT_KEYCHAIN.code.length == 0) {
            revert MissingPrecompile("AccountKeychain", _ACCOUNT_KEYCHAIN);
        }
        if (_TIP403REGISTRY.code.length == 0) {
            revert MissingPrecompile("TIP403Registry", _TIP403REGISTRY);
        }
        if (_TIP20FACTORY.code.length == 0) {
            revert MissingPrecompile("TIP20Factory", _TIP20FACTORY);
        }
        if (_PATH_USD.code.length == 0) {
            revert MissingPrecompile("pathUSD", _PATH_USD);
        }
        if (_STABLECOIN_DEX.code.length == 0) {
            revert MissingPrecompile("StablecoinDEX", _STABLECOIN_DEX);
        }
        if (_FEE_AMM.code.length == 0) {
            revert MissingPrecompile("FeeManager", _FEE_AMM);
        }
        if (_NONCE.code.length == 0) {
            revert MissingPrecompile("Nonce", _NONCE);
        }
        if (_VALIDATOR_CONFIG.code.length == 0) {
            revert MissingPrecompile("ValidatorConfig", _VALIDATOR_CONFIG);
        }

        if (_BLOCKHASH_HISTORY.code.length == 0) {
            revert MissingPrecompile("BlockHashHistory", _BLOCKHASH_HISTORY);
        }

        if (_ZONE_TX_CONTEXT.code.length == 0) {
            MockZoneTxContext mockTxContext = new MockZoneTxContext();
            vm.etch(_ZONE_TX_CONTEXT, address(mockTxContext).code);
        }
        if (_ZONE_TX_CONTEXT.code.length == 0) {
            revert MissingPrecompile("ZoneTxContext", _ZONE_TX_CONTEXT);
        }

        // Set ValidatorConfig owner to sequencer via direct storage write
        // owner is at slot 0 in ValidatorConfig
        vm.store(_VALIDATOR_CONFIG, bytes32(uint256(0)), bytes32(uint256(uint160(sequencer))));

        // Grant DEFAULT_ADMIN_ROLE to pathUSDAdmin
        bytes32 tempoAdminRoleSlot = keccak256(
            abi.encode(
                bytes32(0), // DEFAULT_ADMIN_ROLE
                keccak256(abi.encode(pathUSDAdmin, uint256(0)))
            )
        );
        vm.store(_PATH_USD, tempoAdminRoleSlot, bytes32(uint256(1)));

        token1 = ITIP20Token(
            factory.createToken(
                "TOKEN1", "T1", "USD", ITIP20(_PATH_USD), sequencer, bytes32("token1")
            )
        );
        token2 = ITIP20Token(
            factory.createToken(
                "TOKEN2", "T2", "USD", ITIP20(_PATH_USD), sequencer, bytes32("token2")
            )
        );

        _mockTokenPolicyMigration(_PATH_USD, true);
    }

    function _mockTokenPolicyMigration(address token, bool isSet) internal {
        address[] memory tokens = new address[](1);
        tokens[0] = token;
        vm.mockCall(
            _TIP403REGISTRY,
            abi.encodeCall(ITIP403Registry.migrateTransferPolicyIds, (tokens)),
            abi.encode(isSet ? 1 : 0)
        );
        vm.mockCall(
            _TIP403REGISTRY,
            abi.encodeCall(ITIP403Registry.tokenTransferPolicyId, (token)),
            abi.encode(isSet, uint64(1))
        );
    }

    function _zoneGateways() internal view returns (address[] memory gateways) {
        gateways = new address[](1);
        gateways[0] = address(zoneGateway);
    }

    function _closedLoopAccounts() internal view returns (address[] memory accounts) {
        accounts = new address[](5);
        accounts[0] = address(this);
        accounts[1] = admin;
        accounts[2] = alice;
        accounts[3] = bob;
        accounts[4] = charlie;
    }

    /// @notice Installs the shared runtimes managed by the native TIP-1091 factory.
    function _installSharedZoneRuntimes() internal {
        vm.etch(ZONE_VERIFIER_ADDRESS, type(Verifier).runtimeCode);
        vm.etch(ZONE_MESSENGER_ADDRESS, type(ZoneMessenger).runtimeCode);
    }

    /// @notice Creates a direct portal fixture with native-factory-equivalent storage.
    /// @dev Native ZoneFactory behavior is tested in Tempo. Solidity behavior tests use a direct
    ///      implementation because vanilla Forge cannot execute the Rust precompile.
    function _createZonePortal(
        uint32 zoneId,
        address initialToken,
        address portalAdmin,
        address[] memory sequencers,
        uint8 threshold,
        string memory rpcUrl
    )
        internal
        returns (ZonePortal portal)
    {
        _installSharedZoneRuntimes();
        portal = new ZonePortal();
        vm.prank(ZONE_FACTORY_ADDRESS);
        portal.initialize(
            zoneId,
            initialToken,
            true,
            true,
            _closedLoopAccounts(),
            _zoneGateways(),
            ZONE_MESSENGER_ADDRESS,
            portalAdmin,
            sequencers,
            threshold,
            ZONE_VERIFIER_ADDRESS,
            rpcUrl
        );

        vm.mockCall(
            ZONE_FACTORY_ADDRESS,
            abi.encodeCall(IZoneFactory.zones, (zoneId)),
            abi.encode(
                ZoneInfo({
                    zoneId: zoneId,
                    portal: address(portal),
                    accessMode: true,
                    gatewayMode: true,
                    admin: portalAdmin,
                    sequencers: sequencers,
                    threshold: threshold,
                    verifier: ZONE_VERIFIER_ADDRESS,
                    rpcUrl: rpcUrl
                })
            )
        );
    }

    function _singleWithdrawal(Withdrawal memory withdrawal)
        internal
        pure
        returns (Withdrawal[] memory withdrawals)
    {
        withdrawals = new Withdrawal[](1);
        withdrawals[0] = withdrawal;
    }

    /// @notice Submit through the TIP-1091 entrypoint while dedicated certificate tests exercise
    ///         the real signature precompile behavior independently.
    function _submitBatch(
        IZonePortal portal,
        uint64 tempoBlockNumber,
        uint64 recentTempoBlockNumber,
        BlockTransition memory blockTransition,
        DepositQueueTransition memory depositQueueTransition,
        bytes32 withdrawalQueueHash,
        bytes memory verifierConfig,
        bytes memory proof
    )
        internal
    {
        bytes[] memory signatures = new bytes[](1);
        signatures[0] = hex"01";
        vm.mockCall(
            address(StdPrecompiles.SIGNATURE_VERIFIER),
            abi.encodeWithSelector(ISignatureVerifier.recover.selector),
            abi.encode(sequencer)
        );
        uint256 nextZoneHeight = _submittedZoneHeights[address(portal)] + 1;
        portal.submitBatch(
            tempoBlockNumber,
            recentTempoBlockNumber,
            blockTransition,
            depositQueueTransition,
            withdrawalQueueHash,
            verifierConfig,
            proof,
            nextZoneHeight,
            signatures
        );
        _submittedZoneHeights[address(portal)] = nextZoneHeight;
    }

}
