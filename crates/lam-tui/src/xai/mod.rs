//! SuperGrok / X Premium subscription auth for xAI's CLI chat proxy.
//!
//! Uses the shared Grok OAuth client and device-code flow so Lam can draw from
//! a SuperGrok weekly usage pool without a console API key.

mod auth;
mod oauth;
mod proxy;

pub(crate) use auth::{AuthError, XaiCredentials, XaiCredentialStore};
pub(crate) use oauth::{OAuthError, device_login, ensure_fresh};
pub(crate) use proxy::{
    CLI_PROXY_BASE_URL, ProxyAffinityHeaders, proxy_default_headers, xai_auth_source,
};
