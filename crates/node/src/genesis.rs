//! Zone genesis template and L1 anchoring.
//!
//! The bundled template ships the zone predeploys compiled from `specs/ref-impls`.
//! It is standalone: TempoState is anchored at block 0 with a zero block hash and the
//! `tempoPortal` immutables are `Address::ZERO`. [`l1_anchored_genesis`] patches the
//! template so the zone follows a real L1.

use alloy_consensus::Sealable;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, U256, address};
use tempo_primitives::TempoHeader;
use zone_precompiles::{ZONE_FEE_MANAGER_ADDRESS, tempo_state, zone_fee_manager};

/// Bundled zone dev genesis artifact.
pub const GENESIS_TEMPLATE_JSON: &str = include_str!("../assets/zone-dev-genesis.json");

/// TempoState predeploy address.
const TEMPO_STATE_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000000");
/// ZoneInbox predeploy address.
const ZONE_INBOX_ADDRESS: Address = address!("0x1c00000000000000000000000000000000000001");
/// `tempoPortal` immutable occurrences in ZoneInbox deployed bytecode.
const ZONE_INBOX_PORTAL_IMMUTABLES: usize = 5;

/// Parses the bundled zone genesis template.
pub fn genesis_template() -> eyre::Result<Genesis> {
    serde_json::from_str(GENESIS_TEMPLATE_JSON).map_err(Into::into)
}

/// Builds a zone genesis anchored to a real L1 block.
///
/// Applies three patches to the [template](genesis_template):
///
/// 1. **TempoState storage**: `tempoBlockHash` and `tempoBlockNumber` must reflect the
///    L1 block that serves as the zone's genesis anchor. Without this, `finalizeTempo`
///    rejects the first L1 block for parent hash mismatch.
///
/// 2. **`tempoPortal` immutables**: the portal address is embedded in ZoneInbox deployed
///    bytecode as `PUSH32` immutables. The template is compiled with
///    `Address::ZERO`; without this patch, `readTempoStorageSlot` reads L1 state from
///    `Address::ZERO` instead of the portal.
///
/// 3. **Default fee token**: ZoneFeeManager stores the portal's creation-time token in canonical
///    Zone state so fee resolution does not depend on node-local L1 cache state.
///
/// Returns `(genesis, genesis_block_number)`.
pub fn l1_anchored_genesis(
    l1_header: &TempoHeader,
    portal_address: Address,
    default_fee_token: Address,
) -> eyre::Result<(Genesis, u64)> {
    let genesis_block_number = l1_header.inner.number;

    let l1_genesis_hash = l1_header.hash_slow();

    let mut genesis = genesis_template()?;

    // Patch 1: TempoState storage.
    let tempo_state_account = genesis
        .alloc
        .get_mut(&TEMPO_STATE_ADDRESS)
        .ok_or_else(|| eyre::eyre!("TempoState not found in genesis alloc"))?;
    let storage = tempo_state_account
        .storage
        .get_or_insert_with(Default::default);
    storage.insert(
        B256::from(tempo_state::slots::TEMPO_BLOCK_HASH.to_be_bytes()),
        l1_genesis_hash,
    );
    storage.insert(
        B256::from(tempo_state::slots::TEMPO_BLOCK_NUMBER.to_be_bytes()),
        B256::from(U256::from(l1_header.inner.number).to_be_bytes()),
    );

    // Patch 2: portal address immutables in ZoneInbox.
    if !portal_address.is_zero() {
        let needle = [0u8; 32]; // Address::ZERO left-padded to 32 bytes
        let mut replacement = [0u8; 32];
        replacement[12..].copy_from_slice(portal_address.as_slice());

        let contracts_to_patch: &[(Address, usize)] =
            &[(ZONE_INBOX_ADDRESS, ZONE_INBOX_PORTAL_IMMUTABLES)];

        for &(addr, expected_count) in contracts_to_patch {
            let account = genesis
                .alloc
                .get_mut(&addr)
                .ok_or_else(|| eyre::eyre!("contract {addr} missing in genesis alloc"))?;
            if let Some(code) = &account.code {
                let mut buf = code.to_vec();
                let count = patch_bytes(&mut buf, &needle, &replacement);
                eyre::ensure!(
                    count == expected_count,
                    "expected {expected_count} tempoPortal immutable(s) in {addr}, found {count}: \
                     contract bytecode may have changed, update the expected count"
                );
                account.code = Some(buf.into());
            }
        }
    }

    // Patch 3: canonical default fee token.
    let fee_manager_account = genesis
        .alloc
        .get_mut(&ZONE_FEE_MANAGER_ADDRESS)
        .ok_or_else(|| eyre::eyre!("ZoneFeeManager not found in genesis alloc"))?;
    fee_manager_account
        .storage
        .get_or_insert_with(Default::default)
        .insert(
            B256::from(zone_fee_manager::slots::DEFAULT_FEE_TOKEN.to_be_bytes()),
            B256::left_padding_from(default_fee_token.as_slice()),
        );

    Ok((genesis, genesis_block_number))
}

/// Replaces all non-overlapping occurrences of `needle` with `replacement` in `buf`.
///
/// Both must have the same length. Returns the number of replacements made.
fn patch_bytes(buf: &mut [u8], needle: &[u8], replacement: &[u8]) -> usize {
    assert_eq!(needle.len(), replacement.len());
    let len = needle.len();
    let mut count = 0;
    let mut i = 0;
    while i + len <= buf.len() {
        if buf[i..i + len] == *needle {
            buf[i..i + len].copy_from_slice(replacement);
            count += 1;
            i += len;
        } else {
            i += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempo_contracts::precompiles::PATH_USD_ADDRESS;

    #[test]
    fn patch_bytes_replaces_non_overlapping_occurrences() {
        let mut buf = vec![0, 0, 1, 0, 0, 2, 0, 0];
        let count = patch_bytes(&mut buf, &[0, 0], &[9, 9]);
        assert_eq!(count, 3);
        assert_eq!(buf, vec![9, 9, 1, 9, 9, 2, 9, 9]);
    }

    #[test]
    fn template_has_default_fee_token() {
        let genesis = genesis_template().unwrap();
        let fee_manager_storage = genesis.alloc[&ZONE_FEE_MANAGER_ADDRESS]
            .storage
            .as_ref()
            .unwrap();
        assert_eq!(
            fee_manager_storage
                [&B256::from(zone_fee_manager::slots::DEFAULT_FEE_TOKEN.to_be_bytes())],
            B256::left_padding_from(PATH_USD_ADDRESS.as_slice()),
        );
    }

    #[test]
    fn anchored_genesis_patches_state_and_immutables() {
        let l1_header = TempoHeader::default();
        let portal = address!("0x00000000000000000000000000000000deadbeef");
        let default_fee_token = address!("0x20c0000000000000000000000000000000001234");

        let (genesis, genesis_block_number) =
            l1_anchored_genesis(&l1_header, portal, default_fee_token).unwrap();
        assert_eq!(genesis_block_number, l1_header.inner.number);

        let storage = genesis.alloc[&TEMPO_STATE_ADDRESS]
            .storage
            .as_ref()
            .unwrap();
        assert_eq!(
            storage[&B256::from(tempo_state::slots::TEMPO_BLOCK_HASH.to_be_bytes())],
            l1_header.hash_slow(),
        );

        let fee_manager_storage = genesis.alloc[&ZONE_FEE_MANAGER_ADDRESS]
            .storage
            .as_ref()
            .unwrap();
        assert_eq!(
            fee_manager_storage
                [&B256::from(zone_fee_manager::slots::DEFAULT_FEE_TOKEN.to_be_bytes())],
            B256::left_padding_from(default_fee_token.as_slice()),
        );

        let mut expected = [0u8; 32];
        expected[12..].copy_from_slice(portal.as_slice());
        let addr = ZONE_INBOX_ADDRESS;
        let code = genesis.alloc[&addr].code.as_ref().unwrap();
        assert!(
            code.windows(32).any(|window| window == expected),
            "patched portal immutable missing in {addr}"
        );
    }
}
