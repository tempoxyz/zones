// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import { ITIP20Factory, ITIP20IssuerAccess, ITIP20Metadata } from "../interfaces/external/tempo/ITIP20Factory.sol";
import { Clones } from "@openzeppelin/contracts/proxy/Clones.sol";
import { ERC1967Proxy } from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import { IEarnEngine } from "../interfaces/engines/IEarnEngine.sol";
import { EarnVaultControls, EngineMigrationMode } from "../interfaces/core/EarnVaultTypes.sol";
import { EarnFeesInit } from "../interfaces/core/EarnFeeTypes.sol";
import { IEarnFees } from "../interfaces/core/IEarnFees.sol";
import { EarnFees } from "./EarnFees.sol";
import { EarnVault } from "./EarnVault.sol";

/// @notice Atomic deployer for canonical Earn v1 stacks.
/// @dev One call deploys the vault-backed TIP20 EarnShare and its `EarnVault`, then wires roles so
///      the supply invariant cannot be misconfigured: `EarnVault` receives the EXCLUSIVE TIP20
///      issuer (mint/burn) role and the EarnShare admin is handed to the configured owner. The
///      EarnVault is wired to `params.engine` as its INITIAL engine. The deployment-fixed migration mode
///      determines whether the operator may later swap that engine or users must opt into a replacement
///      Earn stack individually.
///
///      ROUTER IS DEPLOYED SEPARATELY (DELIBERATE). One universal `EarnRouter` serves the Tempo
///      network, holds no token roles or customer configuration, and needs no factory involvement.
///      The atomic guarantee the factory exists to protect is the ISSUER wiring (the EarnVault is the
///      sole minter/burner; deployer admin revoked; owner-admin retained).
///
///      WHY THE SPLIT: embedding the EarnVault implementation in this factory pushes deployment gas beyond
///      Tempo's per-transaction limit. The deployer supplies one separately deployed, locked implementation;
///      the factory fixes that address immutably and creates non-upgradeable ERC-1967 proxies for each stack,
///      preserving the atomic token+EarnVault+issuer story. A caller-chosen `deploymentId` gives each
///      logical stack a stable deterministic namespace. The initial engine, owner, and fees also bind
///      the salt so an unrelated configuration cannot squat that namespace, while separately
///      deployed, replaceable periphery does not affect the EarnShare address.
contract EarnFactory {
    struct DeployParams {
        bytes32 deploymentId;
        address engine;
        address owner;
        EarnVaultControls controls;
        EarnFeesInit fees;
    }

    bytes32 public constant DEFAULT_ADMIN_ROLE = bytes32(0);
    bytes32 public constant EARN_SHARE_SALT_DOMAIN = keccak256("EarnFactory.earnShare.v1");
    bytes32 public constant FEES_SALT_DOMAIN = keccak256("EarnFactory.fees.v1");

    address public immutable tip20Factory;
    address public immutable earnVaultImplementation;
    address public immutable earnFeesImplementation;

    event EarnStackDeployed(
        address indexed earnVault,
        address indexed earnShare,
        address indexed earnFees,
        address engine,
        address asset,
        address owner,
        bytes32 deploymentId,
        address emergencyGuardian,
        address asyncJanitor,
        EngineMigrationMode migrationMode,
        bytes32 earnShareSalt,
        bytes32 controlConfigHash,
        bytes32 feeConfigHash,
        bytes32 earnFeesSalt
    );

    error AdminHandoffFailed();
    error EmptyDeploymentId();
    error EmptyEarnShareMetadata();
    error FactoryCannotBeFinalOwner();
    error InvalidEarnVaultImplementation();
    error InvalidEarnFeesImplementation();
    error IssuerGrantFailed();
    error EarnShareAlreadyExists(address earnShare);
    error EarnShareSupplyNotZero();
    error ZeroAddress();

    constructor(address tip20Factory_, address earnVaultImplementation_, address earnFeesImplementation_) {
        if (
            tip20Factory_ == address(0) || earnVaultImplementation_ == address(0)
                || earnFeesImplementation_ == address(0)
        ) revert ZeroAddress();
        if (earnVaultImplementation_.code.length == 0) revert InvalidEarnVaultImplementation();
        if (earnFeesImplementation_.code.length == 0) revert InvalidEarnFeesImplementation();
        tip20Factory = tip20Factory_;
        earnVaultImplementation = earnVaultImplementation_;
        earnFeesImplementation = earnFeesImplementation_;
    }

    /// @notice Atomically create the EarnShare and its `EarnVault`, grant the EarnVault the sole
    ///         issuer role, and hand EarnShare admin to `params.owner`. `EarnRouter` is deployed
    ///         separately (see contract NatSpec) with the factory-produced EarnVault.
    /// @dev The EarnShare salt is derived from `deploymentId` and the immutable stack configuration,
    ///      so only an exact copy can preempt the predicted token address. `params.owner` is also the
    ///      EarnVault's operator seat. Emergency pause and async-cancellation authority are
    ///      separately bounded by `params.controls`.
    function deploy(DeployParams calldata params)
        external
        returns (address earnShare, EarnVault earnVault, EarnFees earnFees)
    {
        _validateParams(params);

        address asset = IEarnEngine(params.engine).asset();
        if (asset == address(0)) revert ZeroAddress();
        string memory engineName = IEarnEngine(params.engine).name();
        string memory engineSymbol = IEarnEngine(params.engine).symbol();
        string memory earnCurrency = ITIP20Metadata(asset).currency();
        if (bytes(engineName).length == 0 || bytes(engineSymbol).length == 0 || bytes(earnCurrency).length == 0) {
            revert EmptyEarnShareMetadata();
        }

        bytes32 controlConfigHash = keccak256(abi.encode(params.controls));
        bytes32 feeConfigHash = keccak256(abi.encode(params.fees));
        bytes32 earnShareSalt =
            _earnShareSalt(params.deploymentId, params.engine, params.owner, controlConfigHash, feeConfigHash);
        address predictedEarnShare = ITIP20Factory(tip20Factory).getTokenAddress(address(this), earnShareSalt);
        if (predictedEarnShare.code.length != 0) revert EarnShareAlreadyExists(predictedEarnShare);

        earnShare = ITIP20Factory(tip20Factory)
            .createToken(
                string.concat(engineName, " (Earn)"),
                string.concat(engineSymbol, "E"),
                earnCurrency,
                asset,
                address(this),
                earnShareSalt
            );
        if (earnShare == address(0)) revert ZeroAddress();
        if (ITIP20IssuerAccess(earnShare).totalSupply() != 0) revert EarnShareSupplyNotZero();

        earnVault = EarnVault(address(new ERC1967Proxy(earnVaultImplementation, bytes(""))));
        bytes32 earnFeesSalt = _earnFeesSalt(earnShareSalt);
        earnFees = EarnFees(Clones.cloneDeterministic(earnFeesImplementation, earnFeesSalt));
        IEarnFees(address(earnFees)).initialize(address(earnVault), earnShare, params.fees);
        earnVault.initialize(params.engine, earnShare, address(earnFees), params.owner, params.controls, params.fees);

        _grantEarnVaultIssuerRole(earnShare, address(earnVault));
        _handoffShareAdmin(earnShare, params.owner);

        emit EarnStackDeployed(
            address(earnVault),
            earnShare,
            address(earnFees),
            params.engine,
            asset,
            params.owner,
            params.deploymentId,
            params.controls.emergencyGuardian,
            params.controls.asyncJanitor,
            params.controls.migrationMode,
            earnShareSalt,
            controlConfigHash,
            feeConfigHash,
            earnFeesSalt
        );
    }

    /// @notice Computes the EarnShare salt for `params`.
    function computeEarnShareSalt(DeployParams calldata params) external pure returns (bytes32) {
        return _earnShareSalt(
            params.deploymentId,
            params.engine,
            params.owner,
            keccak256(abi.encode(params.controls)),
            keccak256(abi.encode(params.fees))
        );
    }

    /// @notice Predicts the EarnShare address that `deploy` would create for `params`.
    function predictEarnShare(DeployParams calldata params) external view returns (address) {
        return ITIP20Factory(tip20Factory)
            .getTokenAddress(
                address(this),
                _earnShareSalt(
                    params.deploymentId,
                    params.engine,
                    params.owner,
                    keccak256(abi.encode(params.controls)),
                    keccak256(abi.encode(params.fees))
                )
            );
    }

    function predictEarnFees(DeployParams calldata params) external view returns (address) {
        bytes32 earnShareSalt = _earnShareSalt(
            params.deploymentId,
            params.engine,
            params.owner,
            keccak256(abi.encode(params.controls)),
            keccak256(abi.encode(params.fees))
        );
        return Clones.predictDeterministicAddress(earnFeesImplementation, _earnFeesSalt(earnShareSalt), address(this));
    }

    function _validateParams(DeployParams calldata params) internal view {
        if (params.deploymentId == bytes32(0)) revert EmptyDeploymentId();
        if (params.engine == address(0) || params.owner == address(0)) revert ZeroAddress();
        if (params.owner == address(this)) revert FactoryCannotBeFinalOwner();
    }

    function _grantEarnVaultIssuerRole(address earnShare, address earnVault) internal {
        bytes32 issuerRole = ITIP20IssuerAccess(earnShare).ISSUER_ROLE();
        ITIP20IssuerAccess(earnShare).grantRole(issuerRole, earnVault);
        if (!ITIP20IssuerAccess(earnShare).hasRole(earnVault, issuerRole)) revert IssuerGrantFailed();
    }

    function _handoffShareAdmin(address earnShare, address owner) internal {
        ITIP20IssuerAccess(earnShare).grantRole(DEFAULT_ADMIN_ROLE, owner);
        if (!ITIP20IssuerAccess(earnShare).hasRole(owner, DEFAULT_ADMIN_ROLE)) revert AdminHandoffFailed();

        ITIP20IssuerAccess(earnShare).revokeRole(DEFAULT_ADMIN_ROLE, address(this));
        if (ITIP20IssuerAccess(earnShare).hasRole(address(this), DEFAULT_ADMIN_ROLE)) {
            revert AdminHandoffFailed();
        }
    }

    function _earnShareSalt(
        bytes32 deploymentId,
        address engine,
        address owner,
        bytes32 controlConfigHash,
        bytes32 feeConfigHash
    ) internal pure returns (bytes32) {
        return keccak256(
            abi.encode(EARN_SHARE_SALT_DOMAIN, deploymentId, engine, owner, controlConfigHash, feeConfigHash)
        );
    }

    function _earnFeesSalt(bytes32 earnShareSalt) internal pure returns (bytes32) {
        return keccak256(abi.encode(FEES_SALT_DOMAIN, earnShareSalt));
    }
}
