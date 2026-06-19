use super::*;

/// A deposit extracted from L1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Deposit {
    /// TIP-20 token being deposited.
    pub token: Address,
    /// Sender on L1.
    pub sender: Address,
    /// Recipient on the zone.
    pub to: Address,
    /// Net amount deposited (fee already deducted on L1).
    pub amount: u128,
    /// Fee paid on L1.
    pub fee: u128,
    /// Tempo recipient for a failed-deposit refund.
    pub bounceback_recipient: Address,
    /// User-provided memo.
    pub memo: B256,
}

impl Deposit {
    /// Create a new deposit from an event.
    pub fn from_event(event: DepositMade) -> Self {
        Self {
            token: event.token,
            sender: event.sender,
            to: event.to,
            amount: event.netAmount,
            fee: event.fee,
            bounceback_recipient: event.bouncebackRecipient,
            memo: event.memo,
        }
    }

    /// Create a bounce-back deposit from an event.
    pub fn from_bounce_back(event: BounceBack, portal_address: Address) -> Self {
        Self {
            token: event.token,
            sender: portal_address,
            to: event.fallbackRecipient,
            amount: event.amount,
            fee: 0,
            bounceback_recipient: Address::ZERO,
            memo: B256::ZERO,
        }
    }
}

/// An encrypted deposit extracted from L1.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EncryptedDeposit {
    /// TIP-20 token being deposited.
    pub token: Address,
    /// Sender on L1.
    pub sender: Address,
    /// Net amount deposited (fee already deducted on L1).
    pub amount: u128,
    /// Fee paid on L1.
    pub fee: u128,
    /// Tempo recipient for a failed-deposit refund.
    pub bounceback_recipient: Address,
    /// Index of the encryption key used.
    pub key_index: U256,
    /// Ephemeral public key X coordinate.
    pub ephemeral_pubkey_x: B256,
    /// Ephemeral public key Y parity (0x02 or 0x03).
    pub ephemeral_pubkey_y_parity: u8,
    /// AES-256-GCM ciphertext.
    pub ciphertext: Vec<u8>,
    /// GCM nonce (12 bytes).
    pub nonce: [u8; 12],
    /// GCM authentication tag (16 bytes).
    pub tag: [u8; 16],
}

impl EncryptedDeposit {
    /// Create a new encrypted deposit from an event.
    pub fn from_event(event: EncryptedDepositMade) -> Self {
        Self {
            token: event.token,
            sender: event.sender,
            amount: event.netAmount,
            fee: event.fee,
            bounceback_recipient: event.bouncebackRecipient,
            key_index: event.keyIndex,
            ephemeral_pubkey_x: event.ephemeralPubkeyX,
            ephemeral_pubkey_y_parity: event.ephemeralPubkeyYParity,
            ciphertext: event.ciphertext.to_vec(),
            nonce: event.nonce.0,
            tag: event.tag.0,
        }
    }
}

/// A deposit from L1 — either regular (plaintext) or encrypted.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum L1Deposit {
    /// A regular deposit with plaintext recipient and memo.
    Regular(Deposit),
    /// An encrypted deposit where recipient and memo are encrypted.
    Encrypted(EncryptedDeposit),
}

impl L1Deposit {
    /// Compute the next hash chain value: `keccak256(abi.encode(deposit, prevHash))`.
    pub fn hash_chain(&self, prev_hash: B256) -> B256 {
        match self {
            Self::Regular(d) => keccak256(
                (
                    abi::DepositType::Regular,
                    abi::Deposit {
                        token: d.token,
                        sender: d.sender,
                        to: d.to,
                        amount: d.amount,
                        bouncebackRecipient: d.bounceback_recipient,
                        memo: d.memo,
                    },
                    prev_hash,
                )
                    .abi_encode(),
            ),
            Self::Encrypted(d) => keccak256(
                (
                    abi::DepositType::Encrypted,
                    AbiEncryptedDeposit {
                        token: d.token,
                        sender: d.sender,
                        amount: d.amount,
                        bouncebackRecipient: d.bounceback_recipient,
                        keyIndex: d.key_index,
                        encrypted: AbiEncryptedDepositPayload {
                            ephemeralPubkeyX: d.ephemeral_pubkey_x,
                            ephemeralPubkeyYParity: d.ephemeral_pubkey_y_parity,
                            ciphertext: d.ciphertext.clone().into(),
                            nonce: d.nonce.into(),
                            tag: d.tag.into(),
                        },
                    },
                    prev_hash,
                )
                    .abi_encode(),
            ),
        }
    }
}
