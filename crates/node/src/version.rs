use std::{borrow::Cow, env};

use reth_node_core::version::{
    RethCliVersionConsts, try_init_version_metadata, version_metadata as global_version_metadata,
};

#[cfg(zone_stamped_revision)]
mod stamped;

/// Sets version information for Tempo Zone globally.
///
/// The version information is read by the CLI and node services.
pub fn init_version_metadata() {
    try_init_version_metadata(version_metadata())
        .expect("Version metadata should be generated in `build.rs`");
}

/// The globally configured Tempo Zone client identity.
pub fn client_version() -> &'static str {
    global_version_metadata().p2p_client_version.as_ref()
}

/// The version information for Tempo Zone.
pub fn version_metadata() -> RethCliVersionConsts {
    let metadata = RethCliVersionConsts {
        name_client: Cow::Borrowed("Tempo Zone"),
        cargo_pkg_version: Cow::Borrowed(env!("CARGO_PKG_VERSION")),
        vergen_git_sha_long: Cow::Borrowed(env!("VERGEN_GIT_SHA")),
        vergen_git_sha: Cow::Borrowed(env!("VERGEN_GIT_SHA_SHORT")),
        vergen_build_timestamp: Cow::Borrowed(env!("VERGEN_BUILD_TIMESTAMP")),
        vergen_cargo_target_triple: Cow::Borrowed(env!("VERGEN_CARGO_TARGET_TRIPLE")),
        vergen_cargo_features: Cow::Borrowed(env!("VERGEN_CARGO_FEATURES")),
        short_version: Cow::Borrowed(env!("RETH_SHORT_VERSION")),
        long_version: Cow::Owned(format!(
            "{}\n{}\n{}\n{}\n{}",
            env!("RETH_LONG_VERSION_0"),
            env!("RETH_LONG_VERSION_1"),
            env!("RETH_LONG_VERSION_2"),
            env!("RETH_LONG_VERSION_3"),
            env!("RETH_LONG_VERSION_4"),
        )),
        build_profile_name: Cow::Borrowed(env!("RETH_BUILD_PROFILE")),
        p2p_client_version: Cow::Borrowed(env!("RETH_P2P_CLIENT_VERSION")),
        extra_data: Cow::Owned(extra_data()),
    };
    #[cfg(zone_stamped_revision)]
    {
        let mut metadata = metadata;
        set_revision(&mut metadata, &stamped::revision())
            .expect("binary must be stamped with a full Git SHA before distribution");
        metadata
    }
    #[cfg(not(zone_stamped_revision))]
    metadata
}

fn extra_data() -> String {
    format!(
        "tempo-zone/v{}/{}",
        env!("CARGO_PKG_VERSION"),
        env::consts::OS
    )
}

/// Apply image revision metadata consistently to CLI, RPC, and peer identity.
#[cfg(any(zone_stamped_revision, test))]
fn set_revision(metadata: &mut RethCliVersionConsts, sha: &str) -> Result<(), &'static str> {
    if sha.len() != 40 || !sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("revision must be 40 hexadecimal characters");
    }
    metadata.vergen_git_sha_long = Cow::Owned(sha.to_owned());
    metadata.vergen_git_sha = Cow::Owned(sha[..8].to_owned());
    metadata.short_version = Cow::Owned(format!(
        "{}{} ({})",
        env!("CARGO_PKG_VERSION"),
        env!("RETH_VERSION_SUFFIX"),
        &sha[..7]
    ));
    metadata.long_version = Cow::Owned(format!(
        "{}\nCommit SHA: {sha}\n{}\n{}\n{}",
        env!("RETH_LONG_VERSION_0"),
        env!("RETH_LONG_VERSION_2"),
        env!("RETH_LONG_VERSION_3"),
        env!("RETH_LONG_VERSION_4"),
    ));
    metadata.p2p_client_version = Cow::Owned(format!(
        "tempo-zone/v{}-{}/{}",
        env!("CARGO_PKG_VERSION"),
        &sha[..7],
        env!("VERGEN_CARGO_TARGET_TRIPLE"),
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::OperatorWeb3Api;
    use reth_rpc_api::Web3ApiServer as _;

    #[tokio::test]
    async fn uses_tempo_zone_version_defaults() {
        init_version_metadata();
        let metadata = version_metadata();

        assert_eq!(metadata.name_client, "Tempo Zone");
        assert_eq!(metadata.cargo_pkg_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            metadata.p2p_client_version,
            format!(
                "tempo-zone/v{}-{}/{}",
                env!("CARGO_PKG_VERSION"),
                &env!("VERGEN_GIT_SHA")[..7],
                env!("VERGEN_CARGO_TARGET_TRIPLE")
            )
        );
        assert_eq!(
            metadata.extra_data,
            format!(
                "tempo-zone/v{}/{}",
                env!("CARGO_PKG_VERSION"),
                env::consts::OS
            )
        );
        assert_eq!(client_version(), metadata.p2p_client_version);
        assert_ne!(client_version(), "reth-test");

        let rpc_client_version = OperatorWeb3Api
            .into_rpc()
            .call::<_, String>(
                "web3_clientVersion",
                jsonrpsee::core::EmptyServerParams::new(),
            )
            .await
            .expect("operator client version should be available");
        assert_eq!(rpc_client_version, client_version());
    }

    #[test]
    fn image_revision_updates_all_version_fields() {
        let mut metadata = version_metadata();
        let sha = "1234567890abcdef1234567890abcdef12345678";
        set_revision(&mut metadata, sha).unwrap();
        assert_eq!(metadata.vergen_git_sha_long, sha);
        assert_eq!(metadata.vergen_git_sha, "12345678");
        assert_eq!(
            metadata.short_version,
            format!(
                "{}{} (1234567)",
                env!("CARGO_PKG_VERSION"),
                env!("RETH_VERSION_SUFFIX")
            )
        );
        assert_eq!(
            metadata.long_version,
            format!(
                "{}\nCommit SHA: {sha}\n{}\n{}\n{}",
                env!("RETH_LONG_VERSION_0"),
                env!("RETH_LONG_VERSION_2"),
                env!("RETH_LONG_VERSION_3"),
                env!("RETH_LONG_VERSION_4")
            )
        );
        assert_eq!(
            metadata.p2p_client_version,
            format!(
                "tempo-zone/v{}-1234567/{}",
                env!("CARGO_PKG_VERSION"),
                env!("VERGEN_CARGO_TARGET_TRIPLE")
            )
        );
    }

    #[test]
    fn image_revision_rejects_invalid_sha_without_changing_metadata() {
        let mut metadata = version_metadata();
        let original = metadata.vergen_git_sha_long.clone();
        for sha in [
            "",
            "1234567",
            "a".repeat(39).as_str(),
            "g".repeat(40).as_str(),
            "a".repeat(41).as_str(),
            "é".repeat(20).as_str(),
        ] {
            assert_eq!(
                set_revision(&mut metadata, sha),
                Err("revision must be 40 hexadecimal characters")
            );
            assert_eq!(metadata.vergen_git_sha_long, original);
        }
    }
}
