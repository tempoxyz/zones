//! Shared ABI fragments used across zone precompile wrappers.

alloy_sol_types::sol! {
    /// Generic unauthorized access error used by zone-only wrapper logic.
    error Unauthorized();
}
