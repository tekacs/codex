//! Remote control uses `remote-control/auth.json` when present at startup.
//! Its refreshes and identity changes are independent of inference authentication.
//! An absent file shares the normal manager; an invalid override fails startup.

use codex_login::AuthConfig;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::AuthManagerConfig;
use std::io;
use std::sync::Arc;

pub(crate) async fn resolve(
    config: &impl AuthManagerConfig,
    fallback: &Arc<AuthManager>,
) -> io::Result<Arc<AuthManager>> {
    let home = config.codex_home().join("remote-control");
    if !home.join("auth.json").try_exists()? {
        return Ok(Arc::clone(fallback));
    }
    let auth = AuthConfig {
        codex_home: home,
        auth_credentials_store_mode: AuthCredentialsStoreMode::File,
        keyring_backend_kind: config.auth_keyring_backend_kind(),
        forced_login_method: config.forced_login_method(),
        forced_chatgpt_workspace_id: config.forced_chatgpt_workspace_id(),
        managed_auth_policy: config.managed_auth_policy(),
        chatgpt_base_url: Some(config.chatgpt_base_url()),
        auth_route_config: config.auth_route_config(),
    };
    let manager =
        AuthManager::shared_from_auth_config(auth, /*enable_codex_api_key_env*/ false)
            .await
            .map_err(io::Error::other)?;
    if !manager
        .auth_cached()
        .is_some_and(|auth| auth.uses_codex_backend() && auth.get_account_id().is_some())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "remote-control/auth.json must contain valid ChatGPT credentials",
        ));
    }
    Ok(manager)
}
