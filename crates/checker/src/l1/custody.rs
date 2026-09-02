use std::collections::{BTreeMap, BTreeSet};

use alloy_primitives::{Address, Log, U256};
use tempo_contracts::precompiles::ITIP20;
use zone_l1::is_portal_transfer;

use super::events::{CustodyEffect, L1PortalEvent};
use crate::decode_event;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CustodyMovement {
    pub(crate) inflow: U256,
    pub(crate) outflow: U256,
    pub(crate) unattributed_inflow: U256,
}

impl CustodyMovement {
    /// Add one receipt's movement.
    pub(super) fn merge(&mut self, other: Self) -> eyre::Result<()> {
        let inflow = self
            .inflow
            .checked_add(other.inflow)
            .ok_or_else(|| eyre::eyre!("custody inflow overflow"))?;
        let outflow = self
            .outflow
            .checked_add(other.outflow)
            .ok_or_else(|| eyre::eyre!("custody outflow overflow"))?;
        let unattributed_inflow = self
            .unattributed_inflow
            .checked_add(other.unattributed_inflow)
            .ok_or_else(|| eyre::eyre!("unattributed inflow overflow"))?;
        *self = Self {
            inflow,
            outflow,
            unattributed_inflow,
        };
        Ok(())
    }
}

#[derive(Default)]
struct ExpectedMovement {
    inflow: U256,
    outflow: U256,
}

/// Reconcile one receipt's Portal events and TIP-20 transfers.
pub(super) fn reconcile_receipt<'a>(
    portal: Address,
    events: &[L1PortalEvent],
    logs: impl IntoIterator<Item = &'a Log>,
    block: u64,
    receipt: u64,
) -> eyre::Result<BTreeMap<Address, CustodyMovement>> {
    let add = |value: &mut U256, amount: U256| -> eyre::Result<()> {
        *value = value
            .checked_add(amount)
            .ok_or_else(|| eyre::eyre!("custody movement overflow"))?;
        Ok(())
    };
    let mut expected = BTreeMap::<Address, ExpectedMovement>::new();
    for event in events {
        let CustodyEffect {
            token,
            inflow,
            outflow,
        } = event.custody_effect();
        let movement = expected.entry(token).or_default();
        add(&mut movement.inflow, inflow)?;
        add(&mut movement.outflow, outflow)?;
    }

    let mut observed = BTreeMap::<Address, CustodyMovement>::new();
    for log in logs {
        if !is_portal_transfer(log, portal) {
            continue;
        }
        let transfer = decode_event::<ITIP20::Transfer>(log, "Transfer", block)?;
        let movement = observed.entry(log.address).or_default();
        if transfer.to == portal {
            add(&mut movement.inflow, transfer.amount)?;
        }
        if transfer.from == portal {
            add(&mut movement.outflow, transfer.amount)?;
        }
    }

    let tokens = expected
        .keys()
        .chain(observed.keys())
        .copied()
        .collect::<BTreeSet<_>>();
    for token in tokens {
        let expected = expected.remove(&token).unwrap_or_default();
        let movement = observed.entry(token).or_default();
        eyre::ensure!(
            movement.outflow == expected.outflow,
            "Portal outflow mismatch for {token} in Tempo block {block} receipt {receipt}: expected {}, observed {}",
            expected.outflow,
            movement.outflow
        );
        eyre::ensure!(
            movement.inflow >= expected.inflow,
            "Portal inflow mismatch for {token} in Tempo block {block} receipt {receipt}: expected at least {}, observed {}",
            expected.inflow,
            movement.inflow
        );
        movement.unattributed_inflow = movement.inflow - expected.inflow;
    }
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_sol_types::SolEvent;
    use tempo_primitives::TempoAddressExt;

    fn token(suffix: u8) -> Address {
        let mut bytes = [0u8; 20];
        bytes[..12].copy_from_slice(&Address::TIP20_PREFIX);
        bytes[19] = suffix;
        Address::from(bytes)
    }

    fn transfer(token: Address, from: Address, to: Address, amount: u64) -> Log {
        Log {
            address: token,
            data: ITIP20::Transfer {
                from,
                to,
                amount: U256::from(amount),
            }
            .encode_log_data(),
        }
    }

    #[test]
    fn accepts_and_reports_unattributed_inflow() {
        let portal = Address::repeat_byte(1);
        let token = token(2);
        let sender = Address::repeat_byte(3);
        let donor = Address::repeat_byte(4);
        let admin = Address::repeat_byte(5);
        let event = L1PortalEvent::DepositMade {
            token,
            net_amount: 100,
            fee: 5,
            deposit_number: 1,
        };
        let logs = [
            transfer(token, sender, portal, 105),
            transfer(token, portal, admin, 5),
            transfer(token, donor, portal, 10),
        ];

        let movement = reconcile_receipt(portal, &[event], &logs, 1, 0).unwrap()[&token];
        assert_eq!(movement.inflow, U256::from(115));
        assert_eq!(movement.outflow, U256::from(5));
        assert_eq!(movement.unattributed_inflow, U256::from(10));
    }

    #[test]
    fn rejects_outflow_even_when_inflow_is_larger() {
        let portal = Address::repeat_byte(1);
        let token = token(2);
        let logs = [
            transfer(token, Address::repeat_byte(3), portal, 10),
            transfer(token, portal, Address::repeat_byte(4), 7),
        ];

        assert!(reconcile_receipt(portal, &[], &logs, 1, 0).is_err());
    }

    #[test]
    fn ignores_unrelated_malformed_transfers() {
        let portal = Address::repeat_byte(1);
        let log = Log {
            address: token(2),
            data: alloy_primitives::LogData::new_unchecked(
                vec![
                    ITIP20::Transfer::SIGNATURE_HASH,
                    Address::repeat_byte(3).into_word(),
                    Address::repeat_byte(4).into_word(),
                ],
                Default::default(),
            ),
        };

        assert!(
            reconcile_receipt(portal, &[], [&log], 1, 0)
                .unwrap()
                .is_empty()
        );
    }
}
