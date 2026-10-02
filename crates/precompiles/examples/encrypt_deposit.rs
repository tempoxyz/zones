//! Print canonical encrypted ZonePortal.deposit calldata for local devnet testing.
//!
//! Arguments: portal sender recipient token amount key-x key-parity key-index

use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use tempo_zone_contracts::{DepositPayload, ZonePortal};
use zone_precompiles::ecies::encrypt_deposit;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 8 {
        return Err("usage: encrypt_deposit portal sender recipient token amount key-x key-parity key-index".into());
    }
    let portal: Address = args[0].parse()?;
    let sender: Address = args[1].parse()?;
    let recipient: Address = args[2].parse()?;
    let token: Address = args[3].parse()?;
    let amount: u128 = args[4].parse()?;
    let key_x: B256 = args[5].parse()?;
    let key_parity: u8 = args[6].parse()?;
    let key_index: U256 = args[7].parse()?;
    let encrypted = encrypt_deposit(
        &key_x,
        key_parity,
        recipient,
        B256::ZERO,
        sender,
        portal,
        key_index,
    )
    .ok_or("invalid key or failed encryption")?;
    let calldata = ZonePortal::depositCall {
        token,
        amount,
        keyIndex: key_index,
        encrypted: DepositPayload {
            ephemeralPubkeyX: encrypted.eph_pub_x,
            ephemeralPubkeyYParity: encrypted.eph_pub_y_parity,
            ciphertext: Bytes::from(encrypted.ciphertext),
            nonce: encrypted.nonce.into(),
            tag: encrypted.tag.into(),
        },
        tempoRefundRecipient: sender,
    }
    .abi_encode();
    println!("0x{}", alloy_primitives::hex::encode(calldata));
    Ok(())
}
