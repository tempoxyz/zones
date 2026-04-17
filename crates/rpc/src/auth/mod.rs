//! Authorization token parsing and verification.

mod token;

pub use crate::error::AuthError;
pub use token::{
    AuthContext, AuthorizationToken, DEFAULT_MAX_AUTH_TOKEN_VALIDITY,
    DEFAULT_MAX_AUTH_TOKEN_VALIDITY_SECS, X_AUTHORIZATION_TOKEN, build_token_fields,
    parse_auth_header,
};
