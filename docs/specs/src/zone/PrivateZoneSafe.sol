// SPDX-License-Identifier: MIT
pragma solidity ^0.8.13;

/**
 * @title PrivateZoneSafe
 * @notice Locked-down Gnosis Safe singleton for privacy zones.
 *
 * @dev This is a reference specification, not a production implementation. It documents the
 * subset of Gnosis Safe functionality that is safe to deploy on a privacy zone.
 *
 * Removed from standard Safe:
 *
 *   - **Modules**: enableModule, disableModule, execTransactionFromModule, getModules,
 *     isModuleEnabled. Modules can bypass the multisig threshold and execute transactions
 *     autonomously, which expands the attack surface on a privacy zone.
 *
 *   - **Guards**: setGuard. Transaction guards add arbitrary pre/post-execution hooks that
 *     could interact with external state in ways that leak information.
 *
 *   - **DELEGATECALL operation**: execTransaction only supports operation=0 (CALL).
 *     DELEGATECALL runs arbitrary code in the Safe's storage context, which on a privacy
 *     zone could be used to read state that should be scoped to the Safe's owners.
 *
 *   - **getStorageAt**: The standard Safe exposes a function to read arbitrary storage slots.
 *     This is removed to reduce the on-chain attack surface. Owners can still read Safe state
 *     via the dedicated view functions.
 *
 *   - **setFallbackHandler**: The fallback handler is set once during setup and cannot be
 *     changed. This prevents post-initialization changes that could introduce view functions
 *     with unintended information leakage.
 *
 *   - **Refund mechanism**: execTransaction's gas refund parameters (gasPrice, gasToken,
 *     refundReceiver) are removed. Refund logic uses variable gas that could create side
 *     channels, and the zone's fixed-fee model makes refunds unnecessary.
 *
 * View functions (getOwners, isOwner, getThreshold, nonce) remain public. On a privacy zone
 * any authenticated user can call these via eth_call. This is an accepted trade-off:
 * getOwners() must be public for the RPC's contract read delegation to work, and the other
 * views expose no information beyond what getOwners() already reveals.
 */
contract PrivateZoneSafe {

    /*//////////////////////////////////////////////////////////////
                                EVENTS
    //////////////////////////////////////////////////////////////*/

    event SafeSetup(address indexed initiator, address[] owners, uint256 threshold, address fallbackHandler);
    event ExecutionSuccess(bytes32 indexed txHash);
    event ExecutionFailure(bytes32 indexed txHash);
    event AddedOwner(address indexed owner);
    event RemovedOwner(address indexed owner);
    event ChangedThreshold(uint256 threshold);

    /*//////////////////////////////////////////////////////////////
                                ERRORS
    //////////////////////////////////////////////////////////////*/

    error AlreadyInitialized();
    error InvalidThreshold();
    error InvalidOwner();
    error DuplicateOwner();
    error OwnerCountBelowThreshold();
    error InvalidSignature();
    error NotEnoughSignatures();
    error HashAlreadyApproved();
    error NotOwner();
    error CallFailed();

    /*//////////////////////////////////////////////////////////////
                               CONSTANTS
    //////////////////////////////////////////////////////////////*/

    address internal constant SENTINEL = address(0x1);
    bytes32 internal constant DOMAIN_SEPARATOR_TYPEHASH =
        keccak256("EIP712Domain(uint256 chainId,address verifyingContract)");
    bytes32 internal constant SAFE_TX_TYPEHASH =
        keccak256("SafeTx(address to,uint256 value,bytes data,uint256 nonce)");

    /*//////////////////////////////////////////////////////////////
                                STATE
    //////////////////////////////////////////////////////////////*/

    /// @dev Linked list of owners: mapping(current => next). SENTINEL is the head.
    mapping(address => address) internal owners;
    uint256 public ownerCount;
    uint256 public threshold;
    uint256 public nonce;

    /// @dev Pre-approved hashes per owner: mapping(owner => mapping(hash => approved)).
    mapping(address => mapping(bytes32 => bool)) public approvedHashes;

    address public fallbackHandler;
    bool internal initialized;

    /*//////////////////////////////////////////////////////////////
                             INITIALIZATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Initialize the Safe with owners, threshold, and fallback handler.
    /// @dev Can only be called once. The proxy factory calls this immediately after CREATE2.
    /// @param _owners Initial owner addresses. Must be non-zero, non-sentinel, and unique.
    /// @param _threshold Number of required confirmations. Must be >= 1 and <= owners.length.
    /// @param _fallbackHandler Address of the fallback handler for view function delegation.
    function setup(address[] calldata _owners, uint256 _threshold, address _fallbackHandler) external {
        if (initialized) revert AlreadyInitialized();
        if (_threshold == 0 || _threshold > _owners.length) revert InvalidThreshold();

        address current = SENTINEL;
        for (uint256 i = _owners.length; i > 0; i--) {
            address owner = _owners[i - 1];
            if (owner == address(0) || owner == SENTINEL || owner == address(this)) revert InvalidOwner();
            if (owners[owner] != address(0)) revert DuplicateOwner();
            owners[owner] = current;
            current = owner;
        }
        owners[SENTINEL] = current;

        ownerCount = _owners.length;
        threshold = _threshold;
        fallbackHandler = _fallbackHandler;
        initialized = true;

        emit SafeSetup(msg.sender, _owners, _threshold, _fallbackHandler);
    }

    /*//////////////////////////////////////////////////////////////
                          TRANSACTION EXECUTION
    //////////////////////////////////////////////////////////////*/

    /// @notice Execute a transaction confirmed by the required number of owners.
    /// @dev Only CALL operations are supported. DELEGATECALL is disabled on privacy zones.
    /// @param to Destination address.
    /// @param value Ether value to send.
    /// @param data Calldata for the inner transaction.
    /// @param signatures Concatenated ECDSA signatures (65 bytes each) or pre-approved hash markers.
    /// @return success True if the inner call succeeded.
    function execTransaction(
        address to,
        uint256 value,
        bytes calldata data,
        bytes calldata signatures
    ) external returns (bool success) {
        bytes32 txHash = getTransactionHash(to, value, data, nonce);
        _checkSignatures(txHash, signatures);
        nonce++;

        (success,) = to.call{value: value}(data);

        if (success) {
            emit ExecutionSuccess(txHash);
        } else {
            emit ExecutionFailure(txHash);
        }
    }

    /*//////////////////////////////////////////////////////////////
                         SIGNATURE VERIFICATION
    //////////////////////////////////////////////////////////////*/

    /// @notice Pre-approve a transaction hash. Used for on-chain confirmations.
    /// @param hashToApprove The Safe transaction hash to approve.
    function approveHash(bytes32 hashToApprove) external {
        if (!isOwner(msg.sender)) revert NotOwner();
        if (approvedHashes[msg.sender][hashToApprove]) revert HashAlreadyApproved();
        approvedHashes[msg.sender][hashToApprove] = true;
    }

    /// @dev Verify that the required number of owners have signed the transaction hash.
    ///      Supports two signature types per the standard Safe encoding:
    ///        - ECDSA (v ∈ {27, 28}): recover signer from (r, s, v).
    ///        - Pre-approved hash (v == 1): r is the owner address, verified against approvedHashes.
    ///      Signers must be provided in ascending address order (prevents duplicates).
    function _checkSignatures(bytes32 txHash, bytes calldata signatures) internal view {
        uint256 requiredSignatures = threshold;
        if (signatures.length < requiredSignatures * 65) revert NotEnoughSignatures();

        address lastOwner = address(0);
        for (uint256 i = 0; i < requiredSignatures; i++) {
            (uint8 v, bytes32 r, bytes32 s) = _splitSignature(signatures, i);

            address signer;
            if (v == 1) {
                signer = address(uint160(uint256(r)));
                if (!approvedHashes[signer][txHash]) revert InvalidSignature();
            } else {
                signer = ecrecover(txHash, v, r, s);
            }

            if (signer == address(0) || !isOwner(signer)) revert InvalidSignature();
            if (signer <= lastOwner) revert InvalidSignature();
            lastOwner = signer;
        }
    }

    function _splitSignature(bytes calldata signatures, uint256 index)
        internal
        pure
        returns (uint8 v, bytes32 r, bytes32 s)
    {
        uint256 offset = index * 65;
        r = bytes32(signatures[offset:offset + 32]);
        s = bytes32(signatures[offset + 32:offset + 64]);
        v = uint8(signatures[offset + 64]);
    }

    /*//////////////////////////////////////////////////////////////
                          OWNER MANAGEMENT
    //////////////////////////////////////////////////////////////*/

    /// @notice Add a new owner and update the threshold.
    /// @dev Must be called via execTransaction (self-call).
    function addOwnerWithThreshold(address owner, uint256 _threshold) external {
        _requireSelfCall();
        if (owner == address(0) || owner == SENTINEL || owner == address(this)) revert InvalidOwner();
        if (owners[owner] != address(0)) revert DuplicateOwner();

        owners[owner] = owners[SENTINEL];
        owners[SENTINEL] = owner;
        ownerCount++;

        emit AddedOwner(owner);

        if (threshold != _threshold) _changeThreshold(_threshold);
    }

    /// @notice Remove an owner and update the threshold.
    /// @dev Must be called via execTransaction (self-call).
    /// @param prevOwner The owner that points to the owner to be removed in the linked list.
    /// @param owner The owner to remove.
    /// @param _threshold New threshold after removal.
    function removeOwner(address prevOwner, address owner, uint256 _threshold) external {
        _requireSelfCall();
        if (owners[prevOwner] != owner) revert InvalidOwner();
        if (owner == SENTINEL) revert InvalidOwner();

        owners[prevOwner] = owners[owner];
        owners[owner] = address(0);
        ownerCount--;

        emit RemovedOwner(owner);

        if (threshold != _threshold) _changeThreshold(_threshold);
    }

    /// @notice Replace an owner with a new address.
    /// @dev Must be called via execTransaction (self-call).
    function swapOwner(address prevOwner, address oldOwner, address newOwner) external {
        _requireSelfCall();
        if (newOwner == address(0) || newOwner == SENTINEL || newOwner == address(this)) revert InvalidOwner();
        if (owners[newOwner] != address(0)) revert DuplicateOwner();
        if (owners[prevOwner] != oldOwner) revert InvalidOwner();
        if (oldOwner == SENTINEL) revert InvalidOwner();

        owners[newOwner] = owners[oldOwner];
        owners[prevOwner] = newOwner;
        owners[oldOwner] = address(0);

        emit RemovedOwner(oldOwner);
        emit AddedOwner(newOwner);
    }

    /// @notice Change the required number of confirmations.
    /// @dev Must be called via execTransaction (self-call).
    function changeThreshold(uint256 _threshold) external {
        _requireSelfCall();
        _changeThreshold(_threshold);
    }

    function _changeThreshold(uint256 _threshold) internal {
        if (_threshold == 0 || _threshold > ownerCount) revert InvalidThreshold();
        threshold = _threshold;
        emit ChangedThreshold(_threshold);
    }

    function _requireSelfCall() internal view {
        if (msg.sender != address(this)) revert CallFailed();
    }

    /*//////////////////////////////////////////////////////////////
                            VIEW FUNCTIONS
    //////////////////////////////////////////////////////////////*/

    /// @notice Returns the list of Safe owners.
    /// @dev Public — required by the zone RPC's contract read delegation mechanism.
    function getOwners() external view returns (address[] memory) {
        address[] memory result = new address[](ownerCount);
        address current = owners[SENTINEL];
        for (uint256 i = 0; i < ownerCount; i++) {
            result[i] = current;
            current = owners[current];
        }
        return result;
    }

    /// @notice Check if an address is a Safe owner.
    function isOwner(address owner) public view returns (bool) {
        return owner != SENTINEL && owners[owner] != address(0);
    }

    /// @notice Compute the EIP-712 transaction hash for signing.
    function getTransactionHash(address to, uint256 value, bytes calldata data, uint256 _nonce)
        public
        view
        returns (bytes32)
    {
        bytes32 domain =
            keccak256(abi.encode(DOMAIN_SEPARATOR_TYPEHASH, block.chainid, address(this)));
        bytes32 safeTxHash =
            keccak256(abi.encode(SAFE_TX_TYPEHASH, to, value, keccak256(data), _nonce));
        return keccak256(abi.encodePacked(bytes1(0x19), bytes1(0x01), domain, safeTxHash));
    }

    /// @notice Returns the EIP-712 domain separator for this Safe.
    function domainSeparator() external view returns (bytes32) {
        return keccak256(abi.encode(DOMAIN_SEPARATOR_TYPEHASH, block.chainid, address(this)));
    }

    /*//////////////////////////////////////////////////////////////
                           FALLBACK / RECEIVE
    //////////////////////////////////////////////////////////////*/

    /// @dev Delegates view function calls to the fallback handler (e.g., EIP-1271 support).
    fallback() external payable {
        address handler = fallbackHandler;
        if (handler == address(0)) return;

        assembly {
            calldatacopy(0, 0, calldatasize())
            let result := staticcall(gas(), handler, 0, calldatasize(), 0, 0)
            returndatacopy(0, 0, returndatasize())
            switch result
            case 0 { revert(0, returndatasize()) }
            default { return(0, returndatasize()) }
        }
    }

    receive() external payable {}
}
