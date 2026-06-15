//! Credential sources that don't live in `config.toml`.
//!
//! Currently just [`xai_oauth`]: sign in with an xAI account (OAuth 2.0 +
//! PKCE) and use the resulting bearer tokens against `https://api.x.ai/v1`.

pub mod xai_oauth;
