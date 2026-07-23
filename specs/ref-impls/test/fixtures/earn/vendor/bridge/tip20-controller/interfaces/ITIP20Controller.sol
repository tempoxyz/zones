// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

/// @title ITIP20Controller
/// @notice Interface for the TIP20Controller contract which manages minting rate limits and
/// allowances for stablecoins backed by a reserve ledger token
/// @dev This contract enforces three types of limits: global cumulative limits, per-transaction
/// limits, and per-minter allowances
interface ITIP20Controller {
    /*//////////////////////////////////////////////////////////////////////////
                                    Errors
    //////////////////////////////////////////////////////////////////////////*/

    /// @notice Thrown when a mint operation would exceed the per-transaction mint limit
    /// @dev The transaction limit caps individual mint operations
    error MintTxnLimitExceeded();

    /// @notice Thrown when a mint operation would exceed the minter's allowance
    /// @dev Each minter has an individual allowance that decrements with each mint
    error MinterAllowanceExceeded();

    /// @notice Thrown when attempting to perform an operation with an amount of zero
    /// @dev This prevents operations that would result in zero value transfers or operations
    /// that would have no effect
    error AmountCannotBeZero();

    /// @notice Thrown when a mint operation would exceed the absolute maximum amount
    /// @dev This prevents operations that would result in an amount exceeding the absolute
    /// maximum amount
    error AmountExceedsAbsoluteMax();

    /// @notice Thrown when attempting to perform an operation with an invalid stablecoin contract
    /// @dev This prevents operations that would result in an invalid stablecoin contract
    error InvalidStablecoinContract();

    /*//////////////////////////////////////////////////////////////////////////
                                    Events
    //////////////////////////////////////////////////////////////////////////*/

    /// @notice Emitted when only the per-transaction mint limit is updated for a stablecoin
    /// @param sender The address that set the limit (must have MINT_RATE_LIMIT_SETTER_ROLE)
    /// @param stablecoinContract The address of the stablecoin contract
    /// @param mintTxnLimit The new per-transaction mint limit
    event TxnMintLimitSet(address indexed sender, address indexed stablecoinContract, uint256 mintTxnLimit);

    /// @notice Emitted when a minter's allowance is set for a stablecoin
    /// @param sender The address that set the allowance (must have MINT_RATE_LIMIT_SETTER_ROLE)
    /// @param stablecoinContract The address of the stablecoin contract
    /// @param minter The address of the minter whose allowance is being set
    /// @param minterAllowance The new allowance for the minter
    event MinterAllowanceSet(
        address indexed sender, address indexed stablecoinContract, address indexed minter, uint256 minterAllowance
    );

    /// @notice Emitted when tokens are minted to a recipient
    /// @param sender The address that initiated the mint operation
    /// @param stablecoinContract The address of the stablecoin contract
    /// @param to The address receiving the minted tokens
    /// @param amount The amount of tokens minted
    event Mint(address indexed sender, address indexed stablecoinContract, address indexed to, uint256 amount);

    /// @notice Emitted when tokens are burned
    /// @param sender The address that initiated the burn operation
    /// @param stablecoinContract The address of the stablecoin contract
    /// @param amount The amount of tokens burned
    event Burn(address indexed sender, address indexed stablecoinContract, uint256 amount);

    /// @notice Emitted when tokens are unwrapped
    /// @param sender The address that initiated the unwrap operation
    /// @param stablecoinContract The address of the wrapped stablecoin contract
    /// @param amount The amount of wrapped tokens unwrapped
    event Unwrap(address indexed sender, address indexed stablecoinContract, uint256 amount);

    /// @notice Emitted when reserve tokens are wrapped into stablecoins
    /// @param sender The address that initiated the wrap operation
    /// @param stablecoinContract The address of the target stablecoin contract
    /// @param to The address to receive the wrapped tokens
    /// @param amount The amount of reserve tokens wrapped
    event Wrap(address indexed sender, address indexed stablecoinContract, address indexed to, uint256 amount);

    /// @notice Emitted when a reserve store is set for a stablecoin
    /// @param sender The address that set the reserve store
    /// @param stablecoinContract The address of the stablecoin contract
    /// @param reserveStore The address of the reserve store
    event ReserveStoreSet(address indexed sender, address indexed stablecoinContract, address indexed reserveStore);

    /*//////////////////////////////////////////////////////////////////////////
                                    Functions
    //////////////////////////////////////////////////////////////////////////*/

    /**
     * @notice Mints stablecoins to a recipient address
     * @dev Checks transaction limit and decrements minter allowance before
     * minting
     * @param stablecoinContract The address of the stablecoin contract to mint from
     * @param to The address to receive the minted tokens
     * @param amount The amount of tokens to mint
     */
    function mint(address stablecoinContract, address to, uint256 amount) external;

    /**
     * @notice Mints stablecoins to a specified address for bridge ecosystem contracts.
     * @dev Callable only by contracts with the BRIDGE_ECOSYSTEM_CONTRACT_ROLE.
     *      Does not enforce minter allowance or per-transaction mint limits.
     * @param stablecoinContract The address of the stablecoin contract to mint from.
     * @param to The recipient address that will receive the minted tokens.
     * @param amount The amount of tokens to mint.
     */
    function mintBridgeEcosystem(address stablecoinContract, address to, uint256 amount) external;

    /**
     * @notice Burns tokens from the sender's balance for a given stablecoin contract
     * @dev Allows the caller to burn their own tokens. Burns the stablecoin and returns
     * reserve ledger tokens from the ReserveStore.
     * @param stablecoinContract The address of the stablecoin contract
     * @param amount The amount of tokens to burn
     */
    function burn(address stablecoinContract, uint256 amount) external;

    /**
     * @notice Unwraps a given amount of a stablecoin for the caller
     * @dev Burns the stablecoin and transfers the underlying reserve tokens from the
     * ReserveStore to the caller.
     * @param stablecoinContract The address of the stablecoin contract
     * @param amount The amount of tokens to unwrap
     */
    function unwrap(address stablecoinContract, uint256 amount) external;

    /**
     * @notice Wraps reserve ledger tokens into the specified stablecoin and sends them to a
     * recipient
     * @dev Transfers reserve tokens from caller to ReserveStore, then mints stablecoins to
     * recipient
     * @param stablecoinContract The address of the target stablecoin contract
     * @param to The address to receive the wrapped tokens
     * @param amount The amount of reserve tokens to wrap
     */
    function wrap(address stablecoinContract, address to, uint256 amount) external;

    /**
     * @notice Sets the per-transaction mint limit for a stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @param mintTxnLimit The per-transaction mint limit to set
     */
    function setTxnMintLimit(address stablecoinContract, uint256 mintTxnLimit) external;

    /**
     * @notice Sets the mint allowance for a specific minter on a stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @param minter The address of the minter
     * @param minterAllowance The allowance amount to set for the minter
     */
    function setMinterAllowance(address stablecoinContract, address minter, uint256 minterAllowance) external;

    /**
     * @notice Sets or overrides the reserve store for a stablecoin contract
     * @dev Reserve stores are auto-deployed lazily if not set. This function allows
     *      pre-configuration or migration to a different reserve store.
     * @param stablecoinContract The address of the stablecoin contract
     * @param reserveStore The address of the reserve store
     */
    function setReserveStore(address stablecoinContract, address reserveStore) external;

    /**
     * @notice Gets the mint allowance for a specific minter on a stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @param minter The address of the minter
     * @return minterAllowance The remaining allowance for the minter
     */
    function getMinterAllowance(address stablecoinContract, address minter)
        external
        view
        returns (uint256 minterAllowance);

    /**
     * @notice Gets the per-transaction mint limit for a specific stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @return mintTxnLimit The per-transaction mint limit
     */
    function getStablecoinTxnMintLimit(address stablecoinContract) external view returns (uint256 mintTxnLimit);

    /**
     * @notice Gets the reserve store for a specific stablecoin contract
     * @param stablecoinContract The address of the stablecoin contract
     * @return reserveStore The address of the reserve store
     */
    function getReserveStore(address stablecoinContract) external view returns (address reserveStore);
}
