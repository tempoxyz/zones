// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import { ReserveStore } from "./ReserveStore.sol";
import { ITIP20Controller } from "./interfaces/ITIP20Controller.sol";
import {
    AccessControlEnumerableUpgradeable
} from "@openzeppelin/contracts-upgradeable/access/extensions/AccessControlEnumerableUpgradeable.sol";
import { UUPSUpgradeable } from "@openzeppelin/contracts-upgradeable/proxy/utils/UUPSUpgradeable.sol";
import { IERC20 } from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import { SafeERC20 } from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import { ITIP20 } from "../../../src/interfaces/external/tempo/ITIP20.sol";

/// @title TIP20Controller
/// @notice A singleton controller contract that manages minting rate limits and allowances for
/// multiple stablecoins backed by a single reserve ledger token
/// @dev Uses ReserveStore contracts to hold reserve ledger tokens for each stablecoin.
///      Each stablecoin has its own ReserveStore to keep ledger tokens separate for reconciliation.
contract TIP20Controller is ITIP20Controller, AccessControlEnumerableUpgradeable, UUPSUpgradeable {
    using SafeERC20 for IERC20;
    using SafeERC20 for ITIP20;

    error ZeroAddress();

    /*//////////////////////////////////////////////////////////////////////////
                                Immutable Variables
    //////////////////////////////////////////////////////////////////////////*/

    address public immutable RESERVE_LEDGER_TOKEN;

    uint256 public immutable ABSOLUTE_MAX = 1_000_000_000 * 1e6;

    /*//////////////////////////////////////////////////////////////////////////
                                Role Constants
    //////////////////////////////////////////////////////////////////////////*/

    bytes32 public constant MINT_RATE_LIMIT_SETTER_ROLE = keccak256("MINT_RATE_LIMIT_SETTER_ROLE");
    bytes32 public constant BURNER_ROLE = keccak256("BURNER_ROLE");
    bytes32 public constant UNWRAPPER_ROLE = keccak256("UNWRAPPER_ROLE");
    bytes32 public constant BRIDGE_ECOSYSTEM_CONTRACT_ROLE = keccak256("BRIDGE_ECOSYSTEM_CONTRACT_ROLE");

    /*//////////////////////////////////////////////////////////////////////////
                                State Variables
    //////////////////////////////////////////////////////////////////////////*/

    /// @notice Maps each stablecoin contract address and user address to the minter allowance for
    /// that user on the stablecoin.
    /// @dev minterAllowances[stablecoinContract][user] = minterAllowance (remaining tokens that can
    /// be minted by the user)
    mapping(address stablecoinContract => mapping(address user => uint256 minterAllowance)) public minterAllowances;

    /// @notice Maps each stablecoin contract address to its respective mint rate limits.
    /// @dev mintRateLimits[stablecoinContract] = MintRateLimit struct (global and per-transaction
    /// mint limits for the stablecoin)
    mapping(address stablecoinContract => uint256 mintTxnLimit) public mintTxnLimits;

    /// @notice Maps stablecoin contract address to its ReserveStore address
    mapping(address stablecoinContract => address reserveStore) public reserveStores;

    /*//////////////////////////////////////////////////////////////////////////
                                    Constructor
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Constructs the TIP20Controller contract
     * @param _reserveLedgerToken The address of the reserve ledger token
     * @param _disableInitializer Whether to disable the initializer (for proxy pattern)
     */
    constructor(address _reserveLedgerToken, bool _disableInitializer) {
        require(_reserveLedgerToken != address(0), ZeroAddress());

        RESERVE_LEDGER_TOKEN = _reserveLedgerToken;

        if (_disableInitializer) {
            _disableInitializers();
        }
    }

    /*//////////////////////////////////////////////////////////////////////////
                                    Initializer
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Initializes the TIP20Controller contract
     * @param _admin The address to be granted the admin role
     */
    function initialize(address _admin) public initializer {
        __AccessControl_init();
        __UUPSUpgradeable_init();

        _grantRole(DEFAULT_ADMIN_ROLE, _admin);
    }

    /*//////////////////////////////////////////////////////////////////////////
                                        Mint
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Mints stablecoins to a recipient address
     * @dev Checks transaction limit and decrements minter allowance before
     * minting
     * @param stablecoinContract The address of the stablecoin contract to mint from
     * @param to The address to receive the minted tokens
     * @param amount The amount of tokens to mint
     */
    function mint(address stablecoinContract, address to, uint256 amount) public {
        require(amount > 0, AmountCannotBeZero());

        uint256 mintTxnLimit = mintTxnLimits[stablecoinContract];
        uint256 minterAllowance = minterAllowances[stablecoinContract][msg.sender];
        require(minterAllowance >= amount, MinterAllowanceExceeded());
        require(mintTxnLimit >= amount, MintTxnLimitExceeded());

        minterAllowances[stablecoinContract][msg.sender] -= amount;

        _mint(stablecoinContract, to, amount);
    }

    /**
     * @notice Mints stablecoins to a specified address for bridge ecosystem contracts.
     * @dev Callable only by contracts with the BRIDGE_ECOSYSTEM_CONTRACT_ROLE.
     *      Does not enforce minter allowance or per-transaction mint limits.
     * @param stablecoinContract The address of the stablecoin contract to mint from.
     * @param to The recipient address that will receive the minted tokens.
     * @param amount The amount of tokens to mint.
     */
    function mintBridgeEcosystem(address stablecoinContract, address to, uint256 amount)
        public
        onlyRole(BRIDGE_ECOSYSTEM_CONTRACT_ROLE)
    {
        _mint(stablecoinContract, to, amount);
    }

    /**
     * @notice Burns tokens from the sender's balance for a given stablecoin contract
     * @dev Burns the stablecoin and also burns the underlying reserve ledger tokens.
     * @param stablecoinContract The address of the stablecoin contract
     * @param amount The amount of tokens to burn
     */
    function burn(address stablecoinContract, uint256 amount) public onlyRole(BURNER_ROLE) {
        IERC20(stablecoinContract).safeTransferFrom(msg.sender, address(this), amount);

        if (stablecoinContract == RESERVE_LEDGER_TOKEN) {
            ITIP20(RESERVE_LEDGER_TOKEN).burn(amount);
        } else {
            address reserveStore = _getOrCreateReserveStore(stablecoinContract);

            ITIP20(stablecoinContract).burn(amount);
            // Transfer reserve tokens from ReserveStore to this contract and burn them
            IERC20(RESERVE_LEDGER_TOKEN).safeTransferFrom(reserveStore, address(this), amount);
            ITIP20(RESERVE_LEDGER_TOKEN).burn(amount);
        }

        emit Burn(msg.sender, stablecoinContract, amount);
    }

    /**
     * @notice Unwraps a given amount of a stablecoin for the caller
     * @dev Burns the stablecoin and transfers the underlying reserve tokens from
     *      the ReserveStore to the caller.
     * @param stablecoinContract The address of the stablecoin contract
     * @param amount The amount of tokens to unwrap
     */
    function unwrap(address stablecoinContract, uint256 amount) public onlyRole(UNWRAPPER_ROLE) {
        require(stablecoinContract != RESERVE_LEDGER_TOKEN, InvalidStablecoinContract());
        address reserveStore = _getOrCreateReserveStore(stablecoinContract);

        // Transfer stablecoin from sender to this contract and burn it
        IERC20(stablecoinContract).safeTransferFrom(msg.sender, address(this), amount);
        ITIP20(stablecoinContract).burn(amount);

        // Transfer reserve tokens from ReserveStore to the sender
        IERC20(RESERVE_LEDGER_TOKEN).safeTransferFrom(reserveStore, msg.sender, amount);

        emit Unwrap(msg.sender, stablecoinContract, amount);
    }

    /**
     * @notice Wraps reserve ledger tokens into the specified stablecoin and sends them to a
     * recipient.
     * @dev Transfers reserve tokens from caller to ReserveStore, then mints stablecoins to
     * recipient.
     * @param stablecoinContract The address of the target stablecoin contract.
     * @param to The address to receive the wrapped tokens.
     * @param amount The amount of reserve tokens to wrap.
     */
    function wrap(address stablecoinContract, address to, uint256 amount) public {
        require(stablecoinContract != RESERVE_LEDGER_TOKEN, InvalidStablecoinContract());
        require(amount > 0, AmountCannotBeZero());

        address reserveStore = _getOrCreateReserveStore(stablecoinContract);

        // Transfer reserve tokens from caller to ReserveStore
        IERC20(RESERVE_LEDGER_TOKEN).safeTransferFrom(msg.sender, reserveStore, amount);

        // Mint stablecoins to recipient
        ITIP20(stablecoinContract).mint(to, amount);

        emit Wrap(msg.sender, stablecoinContract, to, amount);
    }

    /*//////////////////////////////////////////////////////////////////////////
                                Mint Rate Setters
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Sets the per-transaction mint limit for a stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @param mintTxnLimit The per-transaction mint limit to set
     */
    function setTxnMintLimit(address stablecoinContract, uint256 mintTxnLimit)
        public
        onlyRole(MINT_RATE_LIMIT_SETTER_ROLE)
    {
        require(mintTxnLimit < ABSOLUTE_MAX, AmountExceedsAbsoluteMax());
        mintTxnLimits[stablecoinContract] = mintTxnLimit;

        emit TxnMintLimitSet(msg.sender, stablecoinContract, mintTxnLimit);
    }

    /**
     * @notice Sets the mint allowance for a specific minter on a stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @param minter The address of the minter
     * @param minterAllowance The allowance amount to set for the minter
     */
    function setMinterAllowance(address stablecoinContract, address minter, uint256 minterAllowance)
        public
        onlyRole(MINT_RATE_LIMIT_SETTER_ROLE)
    {
        require(minterAllowance < ABSOLUTE_MAX, AmountExceedsAbsoluteMax());
        minterAllowances[stablecoinContract][minter] = minterAllowance;

        emit MinterAllowanceSet(msg.sender, stablecoinContract, minter, minterAllowance);
    }

    /**
     * @notice Sets or overrides the reserve store for a stablecoin contract
     * @dev Reserve stores are auto-deployed lazily if not set. This function allows
     *      pre-configuration or migration to a different reserve store.
     * @param stablecoinContract The address of the stablecoin contract
     * @param reserveStore The address of the reserve store
     */
    function setReserveStore(address stablecoinContract, address reserveStore) public onlyRole(DEFAULT_ADMIN_ROLE) {
        address oldReserveStore = reserveStores[stablecoinContract];

        if (oldReserveStore != address(0)) {
            IERC20(RESERVE_LEDGER_TOKEN)
                .safeTransferFrom(
                    oldReserveStore, reserveStore, IERC20(RESERVE_LEDGER_TOKEN).balanceOf(oldReserveStore)
                );
        }

        reserveStores[stablecoinContract] = reserveStore;

        emit ReserveStoreSet(msg.sender, stablecoinContract, reserveStore);
    }

    /*//////////////////////////////////////////////////////////////////////////
                                Getters
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Gets the mint allowance for a specific minter on a stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @param minter The address of the minter
     * @return minterAllowance The remaining allowance for the minter
     */
    function getMinterAllowance(address stablecoinContract, address minter)
        public
        view
        returns (uint256 minterAllowance)
    {
        return minterAllowances[stablecoinContract][minter];
    }

    /**
     * @notice Gets the per-transaction mint limit for a specific stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @return mintTxnLimit The per-transaction mint limit
     */
    function getStablecoinTxnMintLimit(address stablecoinContract) public view returns (uint256 mintTxnLimit) {
        return mintTxnLimits[stablecoinContract];
    }

    /**
     * @notice Gets the reserve store for a specific stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @return reserveStore The address of the reserve store
     */
    function getReserveStore(address stablecoinContract) public view returns (address reserveStore) {
        return reserveStores[stablecoinContract];
    }

    /*//////////////////////////////////////////////////////////////////////////
                                Upgrade Logic
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Authorizes an upgrade to a new implementation
     * @dev Only callable by admin role, required by UUPS pattern
     * @param newImplementation The address of the new implementation contract
     */
    function _authorizeUpgrade(address newImplementation) internal override onlyRole(DEFAULT_ADMIN_ROLE) { }

    /*//////////////////////////////////////////////////////////////////////////
                                Internal Functions
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Gets the reserve store for a stablecoin, deploying one if it doesn't exist
     * @dev Uses CREATE2 with salt derived from constructor args for deterministic addresses
     * @param stablecoinContract The address of the stablecoin contract
     * @return reserveStore The address of the reserve store
     */
    function _getOrCreateReserveStore(address stablecoinContract) internal returns (address) {
        address reserveStore = reserveStores[stablecoinContract];
        if (reserveStore == address(0)) {
            bytes32 salt = keccak256(abi.encodePacked(RESERVE_LEDGER_TOKEN, address(this), stablecoinContract));
            reserveStore =
                address(new ReserveStore{ salt: salt }(RESERVE_LEDGER_TOKEN, address(this), stablecoinContract));
            reserveStores[stablecoinContract] = reserveStore;
            emit ReserveStoreSet(address(this), stablecoinContract, reserveStore);
        }
        return reserveStore;
    }

    function _mint(address stablecoinContract, address to, uint256 amount) internal {
        require(amount <= ABSOLUTE_MAX, AmountExceedsAbsoluteMax());

        if (stablecoinContract == RESERVE_LEDGER_TOKEN) {
            ITIP20(RESERVE_LEDGER_TOKEN).mint(to, amount);
        } else {
            address reserveStore = _getOrCreateReserveStore(stablecoinContract);

            // Mint reserve tokens to ReserveStore
            ITIP20(RESERVE_LEDGER_TOKEN).mint(reserveStore, amount);

            // Mint stablecoins to recipient
            ITIP20(stablecoinContract).mint(to, amount);
        }

        emit Mint(msg.sender, stablecoinContract, to, amount);
    }
}
