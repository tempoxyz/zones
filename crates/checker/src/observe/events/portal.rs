#![allow(clippy::too_many_arguments)]

//! Strict Portal event decoding.

use alloy_primitives::{B256, Log, b256};
use tempo_zone_contracts::{MAX_SEQUENCERS, ZonePortal as Portal};
use zone_precompiles::ecies::ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE;

use super::{
    ProtocolEventError, preflight_address_array_count, required_topic, strict_decode_interface,
    unsupported, validate_exact_bytes, validate_token_metadata,
};

// Independent topic0 literals. Tests compare them with `SolEvent::SIGNATURE_HASH`.
pub(super) const DEPOSIT_MADE_TOPIC: B256 =
    b256!("51046223e5e0abca942f13a8f3d1c8dfd59c8b6c4f3e64fc2f5bf453767a97ca");
pub(super) const TOKEN_ENABLED_TOPIC: B256 =
    b256!("4ac4dcc08b0c26c3fb6b58c64c1392b7934b1ce6b0382a5986ea5c3de795e053");
pub(super) const BATCH_SUBMITTED_TOPIC: B256 =
    b256!("5a66941dc92cb865480c966eff640c02b1d00d544b74332fd67c6f1cbfccdf39");
pub(super) const WITHDRAWAL_PROCESSED_TOPIC: B256 =
    b256!("65042ea6dad60c26f055e80ec401b3437c854ed586a0704d305bb4e9ea4518cf");
pub(super) const WITHDRAWAL_BOUNCE_BACK_TOPIC: B256 =
    b256!("adf6f2901dd7af2f28a594f47a925894a08d4de10609dff591a80642648775c5");
pub(super) const DEPOSIT_BOUNCE_BACK_TOPIC: B256 =
    b256!("0f7ef08806234f85aaee43d3ba4589c3bc6d5ac3fc8edd56fc3d91cc7553bdcb");
pub(super) const DEPOSIT_BOUNCE_BACK_PENDING_TOPIC: B256 =
    b256!("5fea28d0adb7d877ae3259768f41ad6741aa1784c4475746dd931364f62e68a1");
pub(super) const REFUND_CLAIMED_TOPIC: B256 =
    b256!("ffd3bbab073ab4b2d0792c270104924c14c285a153b9acddabae166395d2eb5c");
pub(super) const BOUNCEBACK_GAS_UPDATED_TOPIC: B256 =
    b256!("66bcd750662bb66118e25a8e421ae73974634d9af2d44fb9e600d250917fe690");
pub(super) const SEQUENCER_ENCRYPTION_KEY_UPDATED_TOPIC: B256 =
    b256!("854006acd102b75cc8cfe8e0fc954c82e22610d6d4b4ba4f0f19e896ed6397f2");
pub(super) const ZONE_GAS_RATE_UPDATED_TOPIC: B256 =
    b256!("c62141e607d6fcbf7d11fd2b6d8e18e5ebef6d3fff8136ca98822801abbaea38");
pub(super) const MAX_TEMPO_GAS_RATE_UPDATED_TOPIC: B256 =
    b256!("ede0c86e4d0b914b0ba2f68c3359e9ccbcdece694913dcbdf50affe96900e1e8");
pub(super) const ADMIN_TRANSFER_STARTED_TOPIC: B256 =
    b256!("e5cd1c804f1c9cc6d7009e4c0fb532f0e2d8863524c3323a6b3790c3f80bf25c");
pub(super) const ADMIN_TRANSFERRED_TOPIC: B256 =
    b256!("f8ccb027dfcd135e000e9d45e6cc2d662578a8825d4c45b5e32e0adf67e79ec6");
pub(super) const ROLE_UPDATED_TOPIC: B256 =
    b256!("2359a069f5d7871f8f60ad861112ebe12dcf2ba55225c32ec04564d494afc69b");
pub(super) const ENFORCEMENT_MODES_UPDATED_TOPIC: B256 =
    b256!("3e5479494e0a078954a7ff8437aeca3bf7519b51a2fc06b3821251147ff9c5f7");
pub(super) const SEQUENCER_SET_UPDATED_TOPIC: B256 =
    b256!("9282e5956b9751944c6e527bb3fa37aed57d3cfb67979c8962f561a194fc0bc5");
pub(super) const LEADER_UPDATED_TOPIC: B256 =
    b256!("0e49bd8bbce34618e6af3bb74d587a65fa2a594df80b7cc21d690ee78c6d7a69");
pub(super) const DEPOSITS_PAUSED_TOPIC: B256 =
    b256!("eb225a736fbfee3f85ccb72bdf84ff0396ab358b7970e2cc351ab3e3fd92358d");
pub(super) const DEPOSITS_RESUMED_TOPIC: B256 =
    b256!("22ab73af03f04a21e91c7923327f99279b7f5d07d9551762c39bccdf051f1fe9");
pub(super) const PORTAL_PAUSED_TOPIC: B256 =
    b256!("477a1104043ff48a8d2126e5d02d0d4977cb0b2ee7cd4030cb8823007db90f36");
pub(super) const PORTAL_RESUMED_TOPIC: B256 =
    b256!("d2c62538708a93d7454226ca65a098ec07211b126fc22bf2accdcae6561fc2aa");
pub(super) const ABDICATION_SCHEDULED_TOPIC: B256 =
    b256!("839069134f9aac5b7c48da0389dd17d6579bb89ebdddc7fbd83bf64af2dd2ef2");
pub(super) const RPC_URL_UPDATED_TOPIC: B256 =
    b256!("f4e00967b25e707df96d88676243b33be84847ef27615af8ef91290b52294fc6");

pub(super) fn decode(log: &Log) -> Result<Option<Portal::ZonePortalEvents>, ProtocolEventError> {
    let topic = required_topic(log)?;
    match topic {
        DEPOSIT_MADE_TOPIC
        | TOKEN_ENABLED_TOPIC
        | BATCH_SUBMITTED_TOPIC
        | WITHDRAWAL_PROCESSED_TOPIC
        | WITHDRAWAL_BOUNCE_BACK_TOPIC
        | DEPOSIT_BOUNCE_BACK_TOPIC
        | DEPOSIT_BOUNCE_BACK_PENDING_TOPIC
        | REFUND_CLAIMED_TOPIC
        | BOUNCEBACK_GAS_UPDATED_TOPIC
        | SEQUENCER_ENCRYPTION_KEY_UPDATED_TOPIC
        | ZONE_GAS_RATE_UPDATED_TOPIC
        | MAX_TEMPO_GAS_RATE_UPDATED_TOPIC
        | ADMIN_TRANSFER_STARTED_TOPIC
        | ADMIN_TRANSFERRED_TOPIC
        | ROLE_UPDATED_TOPIC
        | ENFORCEMENT_MODES_UPDATED_TOPIC
        | SEQUENCER_SET_UPDATED_TOPIC
        | LEADER_UPDATED_TOPIC
        | DEPOSITS_PAUSED_TOPIC
        | DEPOSITS_RESUMED_TOPIC
        | PORTAL_PAUSED_TOPIC
        | PORTAL_RESUMED_TOPIC
        | ABDICATION_SCHEDULED_TOPIC
        | RPC_URL_UPDATED_TOPIC => {}
        _ => return Err(unsupported(log)),
    }

    // `threshold` is the first body word and the address-array offset the
    // second. Guard its count before Alloy allocates the generated Vec.
    if topic == SEQUENCER_SET_UPDATED_TOPIC {
        preflight_address_array_count(log, "SequencerSetUpdated", 1, MAX_SEQUENCERS)?;
    }

    let decoded = strict_decode_interface::<Portal::ZonePortalEvents>(log, "Portal event")?;
    validate_dynamic_bounds(log, &decoded)?;

    let changes_checker_state = match &decoded {
        Portal::ZonePortalEvents::DepositMade(_)
        | Portal::ZonePortalEvents::TokenEnabled(_)
        | Portal::ZonePortalEvents::BatchSubmitted(_)
        | Portal::ZonePortalEvents::WithdrawalProcessed(_)
        | Portal::ZonePortalEvents::WithdrawalBounceBack(_)
        | Portal::ZonePortalEvents::DepositBounceBack(_)
        | Portal::ZonePortalEvents::DepositBounceBackPending(_)
        | Portal::ZonePortalEvents::RefundClaimed(_)
        | Portal::ZonePortalEvents::BouncebackGasUpdated(_) => true,
        Portal::ZonePortalEvents::DepositsPaused(_)
        | Portal::ZonePortalEvents::DepositsResumed(_)
        | Portal::ZonePortalEvents::PortalPaused(_)
        | Portal::ZonePortalEvents::PortalResumed(_)
        | Portal::ZonePortalEvents::AbdicationScheduled(_)
        | Portal::ZonePortalEvents::RpcUrlUpdated(_)
        | Portal::ZonePortalEvents::SequencerEncryptionKeyUpdated(_)
        | Portal::ZonePortalEvents::ZoneGasRateUpdated(_)
        | Portal::ZonePortalEvents::MaxTempoGasRateUpdated(_)
        | Portal::ZonePortalEvents::AdminTransferStarted(_)
        | Portal::ZonePortalEvents::AdminTransferred(_)
        | Portal::ZonePortalEvents::RoleUpdated(_)
        | Portal::ZonePortalEvents::EnforcementModesUpdated(_)
        | Portal::ZonePortalEvents::SequencerSetUpdated(_)
        | Portal::ZonePortalEvents::LeaderUpdated(_) => false,
    };
    Ok(changes_checker_state.then_some(decoded))
}

fn validate_dynamic_bounds(
    log: &Log,
    event: &Portal::ZonePortalEvents,
) -> Result<(), ProtocolEventError> {
    match event {
        Portal::ZonePortalEvents::DepositMade(event) => validate_exact_bytes(
            log,
            "DepositMade",
            "ciphertext",
            event.ciphertext.len(),
            ENCRYPTED_PAYLOAD_PLAINTEXT_SIZE,
        ),
        Portal::ZonePortalEvents::TokenEnabled(event) => validate_token_metadata(
            log,
            "TokenEnabled",
            &event.name,
            &event.symbol,
            &event.currency,
        ),
        Portal::ZonePortalEvents::SequencerSetUpdated(event)
            if event.sequencers.len() > MAX_SEQUENCERS =>
        {
            Err(super::malformed(
                log,
                "SequencerSetUpdated",
                format!(
                    "address array length {} exceeds {MAX_SEQUENCERS}",
                    event.sequencers.len()
                ),
            ))
        }
        _ => Ok(()),
    }
}
