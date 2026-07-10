//! Tempo Zone L2 Node.
#![cfg_attr(not(test), warn(unused_crate_dependencies))]

use zone_node::cli::ZoneCli;

#[global_allocator]
static ALLOC: reth_cli_util::allocator::Allocator = reth_cli_util::allocator::new_allocator();

fn main() {
    reth_cli_util::sigsegv_handler::install();

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls CryptoProvider");

    if std::env::var_os("RUST_BACKTRACE").is_none() {
        unsafe { std::env::set_var("RUST_BACKTRACE", "1") };
    }

    // `dev` is dispatched before the reth CLI: reth's `Cli` has a fixed set of
    // subcommands, so the dev bootstrap is parsed separately.
    let result = if std::env::args().nth(1).as_deref() == Some("dev") {
        zone_node::dev::run(std::env::args_os().skip(1))
    } else {
        ZoneCli::parse().run()
    };

    if let Err(err) = result {
        eprintln!("Error: {err:?}");
        std::process::exit(1);
    }
}
