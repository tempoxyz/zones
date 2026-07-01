// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

import { ZONE_TX_CONTEXT } from "../src/interfaces/IZone.sol";
import { EIP2935 } from "../src/libraries/BlockHashHistory.sol";
import { MockZoneTxContext } from "./mocks/MockZoneTxContext.sol";
import { Test, console } from "forge-std/Test.sol";
import { StdPrecompiles } from "tempo-std/StdPrecompiles.sol";
import { IAccountKeychain } from "tempo-std/interfaces/IAccountKeychain.sol";
import { IFeeManager } from "tempo-std/interfaces/IFeeManager.sol";
import { INonce } from "tempo-std/interfaces/INonce.sol";
import { IStablecoinDEX } from "tempo-std/interfaces/IStablecoinDEX.sol";
import { ITIP20 } from "tempo-std/interfaces/ITIP20.sol";
import { ITIP20Token } from "tempo-std/interfaces/ITIP20.sol";
import { ITIP20Factory } from "tempo-std/interfaces/ITIP20Factory.sol";
import { ITIP403Registry } from "tempo-std/interfaces/ITIP403Registry.sol";
import { IValidatorConfig } from "tempo-std/interfaces/IValidatorConfig.sol";

/// @notice Base test framework for all spec tests
/// pathUSD is just a TIP20 at a special address (0x20C0...) with token_id=0
contract BaseTest is Test {

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

    error MissingPrecompile(string name, address addr);
    error CallShouldHaveReverted();

    function setUp() public virtual {
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
    }

}
