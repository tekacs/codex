use super::pairing_unavailable_error;
use super::protocol::RemoteControlPairingStatusRequest;
use super::protocol::RemoteControlPairingStatusResponse as BackendRemoteControlPairingStatusResponse;
use super::protocol::RemoteControlTarget;
use super::protocol::StartRemoteControlPairingRequest;
use super::protocol::StartRemoteControlPairingResponse;
use axum::http::HeaderMap;
use axum::http::header::USER_AGENT;
use codex_app_server_protocol::RemoteControlPairingStartResponse;
use codex_app_server_protocol::RemoteControlPairingStatusResponse;
use codex_login::default_client::create_client_without_request_logging;
use codex_login::default_client::get_codex_user_agent;
use codex_login::default_client::originator;
use codex_state::RemoteControlEnrollmentRecord;
use codex_state::StateRuntime;
#[cfg(not(test))]
use serde::Deserialize;
use std::io;
use std::io::ErrorKind;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::info;
use tracing::warn;

const REMOTE_CONTROL_PAIRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const REMOTE_CONTROL_RESPONSE_BODY_MAX_BYTES: usize = 4096;
const REMOTE_CONTROL_SERVER_TOKEN_REFRESH_SKEW_SECS: i64 = 5 * 60;
const REMOTE_CONTROL_DESKTOP_ORIGINATOR: &str = "Codex Desktop";
#[cfg(not(test))]
const DESKTOP_APP_INFO_PLIST: &str = "/Applications/Codex.app/Contents/Info.plist";
#[cfg(not(test))]
const PLUTIL_BIN: &str = "/usr/bin/plutil";
#[cfg(not(test))]
const REMOTE_CONTROL_LATEST_RELEASE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(5);
const DEV_BUILD_VERSION_SENTINEL: &str = "0.0.0";
#[cfg(not(test))]
const GITHUB_LATEST_RELEASE_URL: &str = "https://api.github.com/repos/openai/codex/releases/latest";

#[cfg(test)]
const TEST_DESKTOP_APP_SERVER_VERSION: &str = "26.616.41845";
#[cfg(test)]
const TEST_LATEST_STABLE_RELEASE_TAG: &str = "rust-v0.141.0";

const REQUEST_ID_HEADER: &str = "x-request-id";
const OAI_REQUEST_ID_HEADER: &str = "x-oai-request-id";
const CF_RAY_HEADER: &str = "cf-ray";
pub(super) const REMOTE_CONTROL_ORIGINATOR_HEADER: &str = "originator";
static SOURCE_BUILD_REPORTED_APP_SERVER_VERSION: std::sync::OnceLock<String> =
    std::sync::OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RemoteControlEnrollment {
    pub(super) remote_control_target: RemoteControlTarget,
    pub(super) account_id: String,
    pub(super) environment_id: String,
    pub(super) server_id: String,
    pub(super) server_name: String,
    pub(super) remote_control_token: Option<String>,
    pub(super) expires_at: Option<OffsetDateTime>,
    pub(super) next_refresh_at: Option<OffsetDateTime>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteControlServerTokenRefreshRequirement {
    Required,
    Proactive,
    NotNeeded,
}

impl RemoteControlEnrollment {
    pub(super) async fn start_pairing(
        &self,
        request: StartRemoteControlPairingRequest,
    ) -> io::Result<RemoteControlPairingStartResponse> {
        if self.server_token_refresh_requirement()
            == RemoteControlServerTokenRefreshRequirement::Required
        {
            return Err(pairing_unavailable_error());
        }
        let remote_control_token = self
            .remote_control_token
            .as_deref()
            .ok_or_else(pairing_unavailable_error)?;

        let response = create_client_without_request_logging()
            .post(&self.remote_control_target.pair_url)
            .timeout(REMOTE_CONTROL_PAIRING_TIMEOUT)
            .header(
                REMOTE_CONTROL_ORIGINATOR_HEADER,
                remote_control_originator(),
            )
            .header(USER_AGENT, remote_control_user_agent().await)
            .bearer_auth(remote_control_token)
            .json(&request)
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to start remote control pairing at `{}`: {err}",
                    self.remote_control_target.pair_url
                ))
            })?;
        let headers = response.headers().clone();
        let status = response.status();
        let body = response.bytes().await.map_err(|err| {
            io::Error::other(format!(
                "failed to read remote control pairing response from `{}`: {err}",
                self.remote_control_target.pair_url
            ))
        })?;
        let body_preview = preview_remote_control_response_body(&body);
        if !status.is_success() {
            let error_kind = match status.as_u16() {
                401 | 403 => ErrorKind::PermissionDenied,
                404 => ErrorKind::NotFound,
                _ => ErrorKind::Other,
            };
            return Err(io::Error::new(
                error_kind,
                format!(
                    "remote control pairing failed at `{}`: HTTP {status}, {}, body: {body_preview}",
                    self.remote_control_target.pair_url,
                    format_headers(&headers)
                ),
            ));
        }

        let pairing = serde_json::from_slice::<StartRemoteControlPairingResponse>(&body).map_err(
            |err| {
                io::Error::other(format!(
                    "failed to parse remote control pairing response from `{}`: HTTP {status}, {}, body: {body_preview}, decode error: {err}",
                    self.remote_control_target.pair_url,
                    format_headers(&headers)
                ))
            },
        )?;
        let StartRemoteControlPairingResponse {
            pairing_code,
            manual_pairing_code,
            server_id,
            environment_id,
            expires_at,
        } = pairing;
        if server_id != self.server_id || environment_id != self.environment_id {
            return Err(io::Error::other(format!(
                "remote control pairing returned mismatched enrollment: expected server_id={}, environment_id={}; got server_id={}, environment_id={}",
                self.server_id, self.environment_id, server_id, environment_id
            )));
        }
        let expires_at = OffsetDateTime::parse(&expires_at, &Rfc3339)
            .map_err(|err| {
                io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "failed to parse remote control pairing response from `{}`: HTTP {status}, {}, body: {body_preview}, expires_at parse error: {err}",
                        self.remote_control_target.pair_url,
                        format_headers(&headers)
                    ),
                )
            })?
            .unix_timestamp();

        Ok(RemoteControlPairingStartResponse {
            pairing_code,
            manual_pairing_code,
            environment_id,
            expires_at,
        })
    }

    pub(super) async fn pairing_status(
        &self,
        request: RemoteControlPairingStatusRequest,
    ) -> io::Result<RemoteControlPairingStatusResponse> {
        if self.server_token_refresh_requirement()
            == RemoteControlServerTokenRefreshRequirement::Required
        {
            return Err(pairing_unavailable_error());
        }
        let remote_control_token = self
            .remote_control_token
            .as_deref()
            .ok_or_else(pairing_unavailable_error)?;

        let response = create_client_without_request_logging()
            .post(&self.remote_control_target.pair_status_url)
            .timeout(REMOTE_CONTROL_PAIRING_TIMEOUT)
            .header(
                REMOTE_CONTROL_ORIGINATOR_HEADER,
                remote_control_originator(),
            )
            .header(USER_AGENT, remote_control_user_agent().await)
            .bearer_auth(remote_control_token)
            .json(&request)
            .send()
            .await
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to check remote control pairing status at `{}`: {err}",
                    self.remote_control_target.pair_status_url
                ))
            })?;
        let headers = response.headers().clone();
        let status = response.status();
        let body = response.bytes().await.map_err(|err| {
            io::Error::other(format!(
                "failed to read remote control pairing status response from `{}`: {err}",
                self.remote_control_target.pair_status_url
            ))
        })?;
        let body_preview = preview_remote_control_response_body(&body);
        if !status.is_success() {
            let error_kind = match status.as_u16() {
                401 | 403 => ErrorKind::PermissionDenied,
                404 | 410 => ErrorKind::InvalidInput,
                _ => ErrorKind::Other,
            };
            return Err(io::Error::new(
                error_kind,
                format!(
                    "remote control pairing status failed at `{}`: HTTP {status}, {}, body: {body_preview}",
                    self.remote_control_target.pair_status_url,
                    format_headers(&headers)
                ),
            ));
        }

        let response = serde_json::from_slice::<BackendRemoteControlPairingStatusResponse>(&body)
            .map_err(|err| {
                io::Error::other(format!(
                    "failed to parse remote control pairing status response from `{}`: HTTP {status}, {}, body: {body_preview}, decode error: {err}",
                    self.remote_control_target.pair_status_url,
                    format_headers(&headers)
                ))
            })?;
        Ok(RemoteControlPairingStatusResponse {
            claimed: response.claimed,
        })
    }

    pub(super) fn server_token_refresh_requirement(
        &self,
    ) -> RemoteControlServerTokenRefreshRequirement {
        self.server_token_refresh_requirement_at(OffsetDateTime::now_utc())
    }

    pub(super) fn should_refresh_server_token(&self) -> bool {
        self.server_token_refresh_requirement()
            != RemoteControlServerTokenRefreshRequirement::NotNeeded
    }

    pub(super) fn server_token_refresh_requirement_at(
        &self,
        now: OffsetDateTime,
    ) -> RemoteControlServerTokenRefreshRequirement {
        let Some(expires_at) = self.remote_control_token.as_ref().and(self.expires_at) else {
            return RemoteControlServerTokenRefreshRequirement::Required;
        };
        if expires_at <= now {
            return RemoteControlServerTokenRefreshRequirement::Required;
        }
        if expires_at > now + time::Duration::seconds(REMOTE_CONTROL_SERVER_TOKEN_REFRESH_SKEW_SECS)
            || self
                .next_refresh_at
                .is_some_and(|next_refresh_at| next_refresh_at > now)
        {
            return RemoteControlServerTokenRefreshRequirement::NotNeeded;
        }
        RemoteControlServerTokenRefreshRequirement::Proactive
    }

    pub(super) fn clear_server_token(&mut self) {
        self.remote_control_token = None;
        self.expires_at = None;
    }
}

pub(super) async fn load_persisted_remote_control_enrollment(
    state_db: Option<&StateRuntime>,
    remote_control_target: &RemoteControlTarget,
    account_id: &str,
    app_server_client_name: Option<&str>,
) -> io::Result<Option<RemoteControlEnrollment>> {
    let Some(state_db) = state_db else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!(
                "remote control enrollment cache unavailable because sqlite state db is disabled: websocket_url={}, account_id={}, app_server_client_name={:?}",
                remote_control_target.websocket_url, account_id, app_server_client_name
            ),
        ));
    };
    let enrollment = match state_db
        .get_remote_control_enrollment(
            &remote_control_target.websocket_url,
            account_id,
            app_server_client_name,
        )
        .await
    {
        Ok(enrollment) => enrollment,
        Err(err) => {
            warn!(
                "failed to load persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, err={err}",
                remote_control_target.websocket_url, account_id, app_server_client_name
            );
            return Err(io::Error::other(err));
        }
    };

    match enrollment {
        Some(enrollment) => {
            info!(
                "reusing persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, server_id={}, environment_id={}",
                remote_control_target.websocket_url,
                account_id,
                app_server_client_name,
                enrollment.server_id,
                enrollment.environment_id
            );
            Ok(Some(RemoteControlEnrollment {
                remote_control_target: remote_control_target.clone(),
                account_id: enrollment.account_id,
                environment_id: enrollment.environment_id,
                server_id: enrollment.server_id,
                server_name: enrollment.server_name,
                remote_control_token: None,
                expires_at: None,
                next_refresh_at: None,
            }))
        }
        None => {
            info!(
                "no persisted remote control enrollment found: websocket_url={}, account_id={}, app_server_client_name={:?}",
                remote_control_target.websocket_url, account_id, app_server_client_name
            );
            Ok(None)
        }
    }
}

pub(super) async fn update_persisted_remote_control_enrollment(
    state_db: Option<&StateRuntime>,
    remote_control_target: &RemoteControlTarget,
    account_id: &str,
    app_server_client_name: Option<&str>,
    enrollment: Option<&RemoteControlEnrollment>,
    remote_control_enabled: Option<bool>,
) -> io::Result<()> {
    let Some(state_db) = state_db else {
        return Err(io::Error::new(
            ErrorKind::NotFound,
            format!(
                "remote control enrollment persistence unavailable because sqlite state db is disabled: websocket_url={}, account_id={}, app_server_client_name={:?}, has_enrollment={}",
                remote_control_target.websocket_url,
                account_id,
                app_server_client_name,
                enrollment.is_some()
            ),
        ));
    };
    if let &Some(enrollment) = &enrollment
        && enrollment.account_id != account_id
    {
        return Err(io::Error::other(format!(
            "enrollment account_id does not match expected account_id `{account_id}`"
        )));
    }

    if let Some(enrollment) = enrollment {
        state_db
            .upsert_remote_control_enrollment(&RemoteControlEnrollmentRecord {
                websocket_url: remote_control_target.websocket_url.clone(),
                account_id: account_id.to_string(),
                app_server_client_name: app_server_client_name.map(str::to_string),
                server_id: enrollment.server_id.clone(),
                environment_id: enrollment.environment_id.clone(),
                server_name: enrollment.server_name.clone(),
                remote_control_enabled,
            })
            .await
            .map_err(io::Error::other)?;
        info!(
            "persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, server_id={}, environment_id={}",
            remote_control_target.websocket_url,
            account_id,
            app_server_client_name,
            enrollment.server_id,
            enrollment.environment_id
        );
        Ok(())
    } else {
        let rows_affected = state_db
            .delete_remote_control_enrollment(
                &remote_control_target.websocket_url,
                account_id,
                app_server_client_name,
            )
            .await
            .map_err(io::Error::other)?;
        info!(
            "cleared persisted remote control enrollment: websocket_url={}, account_id={}, app_server_client_name={:?}, rows_affected={rows_affected}",
            remote_control_target.websocket_url, account_id, app_server_client_name
        );
        Ok(())
    }
}

pub(crate) fn preview_remote_control_response_body(body: &[u8]) -> String {
    let body = String::from_utf8_lossy(body);
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "<empty>".to_string();
    }
    let redacted = redact_remote_control_response_body(trimmed);
    if redacted.len() <= REMOTE_CONTROL_RESPONSE_BODY_MAX_BYTES {
        return redacted;
    }

    let mut cut = REMOTE_CONTROL_RESPONSE_BODY_MAX_BYTES;
    while !redacted.is_char_boundary(cut) {
        cut = cut.saturating_sub(1);
    }
    let mut truncated = redacted[..cut].to_string();
    truncated.push_str("...");
    truncated
}

fn redact_remote_control_response_body(body: &str) -> String {
    let Ok(mut body_json) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let Some(body_object) = body_json.as_object_mut() else {
        return body.to_string();
    };
    for sensitive_field in [
        "remote_control_token",
        "pairing_code",
        "manual_pairing_code",
    ] {
        if let Some(value) = body_object.get_mut(sensitive_field) {
            *value = serde_json::Value::String("<redacted>".to_string());
        }
    }
    body_json.to_string()
}

pub(crate) fn format_headers(headers: &HeaderMap) -> String {
    let request_id_str = headers
        .get(REQUEST_ID_HEADER)
        .or_else(|| headers.get(OAI_REQUEST_ID_HEADER))
        .map(|value| value.to_str().unwrap_or("<invalid utf-8>").to_owned())
        .unwrap_or_else(|| "<none>".to_owned());
    let cf_ray_str = headers
        .get(CF_RAY_HEADER)
        .map(|value| value.to_str().unwrap_or("<invalid utf-8>").to_owned())
        .unwrap_or_else(|| "<none>".to_owned());
    format!("request-id: {request_id_str}, cf-ray: {cf_ray_str}")
}

pub(super) async fn reported_app_server_version() -> String {
    let build_version = env!("CARGO_PKG_VERSION");
    if build_version != DEV_BUILD_VERSION_SENTINEL {
        return build_version.to_string();
    }

    if let Some(version) = SOURCE_BUILD_REPORTED_APP_SERVER_VERSION.get() {
        return version.clone();
    }

    let version = match installed_desktop_app_server_version().await {
        Ok(version) => version,
        Err(desktop_err) => {
            warn!(
                "failed to resolve installed Desktop app-server version for remote-control enrollment: {desktop_err}"
            );
            match latest_stable_release_version().await {
                Ok(version) => version,
                Err(release_err) => {
                    warn!(
                        "failed to resolve latest stable release version for remote-control enrollment: {release_err}"
                    );
                    build_version.to_string()
                }
            }
        }
    };
    let _ = SOURCE_BUILD_REPORTED_APP_SERVER_VERSION.set(version.clone());
    version
}

pub(super) async fn remote_control_user_agent() -> String {
    let version = reported_app_server_version().await;
    remote_control_user_agent_with_version(&version)
}

pub(super) fn cached_remote_control_user_agent() -> Option<String> {
    if env!("CARGO_PKG_VERSION") == DEV_BUILD_VERSION_SENTINEL {
        return SOURCE_BUILD_REPORTED_APP_SERVER_VERSION
            .get()
            .map(|version| remote_control_user_agent_with_version(version));
    }

    Some(remote_control_user_agent_with_version(env!(
        "CARGO_PKG_VERSION"
    )))
}

pub(super) fn remote_control_originator() -> String {
    if env!("CARGO_PKG_VERSION") == DEV_BUILD_VERSION_SENTINEL {
        REMOTE_CONTROL_DESKTOP_ORIGINATOR.to_string()
    } else {
        originator().value
    }
}

fn remote_control_user_agent_with_version(version: &str) -> String {
    let build_version = env!("CARGO_PKG_VERSION");
    let mut current = get_codex_user_agent();
    if build_version == DEV_BUILD_VERSION_SENTINEL
        && let Some((_, rest)) = current.split_once('/')
    {
        current = format!("{REMOTE_CONTROL_DESKTOP_ORIGINATOR}/{rest}");
    }
    if build_version != version {
        let build_marker = format!("/{build_version} ");
        let replacement = format!("/{version} ");
        current = current.replacen(&build_marker, &replacement, 1);
    }
    current
}

#[cfg(not(test))]
async fn installed_desktop_app_server_version() -> io::Result<String> {
    let output = std::process::Command::new(PLUTIL_BIN)
        .args([
            "-extract",
            "CFBundleShortVersionString",
            "raw",
            "-o",
            "-",
            DESKTOP_APP_INFO_PLIST,
        ])
        .output()
        .map_err(|err| {
            io::Error::other(format!(
                "failed to read installed Desktop Codex bundle version from `{DESKTOP_APP_INFO_PLIST}`: {err}"
            ))
        })?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "reading installed Desktop Codex bundle version from `{DESKTOP_APP_INFO_PLIST}` exited with {}",
            output.status
        )));
    }

    let stdout = String::from_utf8(output.stdout).map_err(|err| {
        io::Error::other(format!(
            "installed Desktop Codex bundle version from `{DESKTOP_APP_INFO_PLIST}` returned non-UTF8 output: {err}"
        ))
    })?;
    extract_desktop_app_bundle_version(&stdout)
}

#[cfg(test)]
async fn installed_desktop_app_server_version() -> io::Result<String> {
    Ok(TEST_DESKTOP_APP_SERVER_VERSION.to_string())
}

fn extract_desktop_app_bundle_version(output: &str) -> io::Result<String> {
    let version = output.trim();
    if version.is_empty() {
        return Err(io::Error::other(
            "installed Desktop Codex bundle version output was empty",
        ));
    }
    Ok(version.to_string())
}

#[cfg(not(test))]
async fn latest_stable_release_version() -> io::Result<String> {
    #[derive(Deserialize)]
    struct ReleaseInfo {
        tag_name: String,
    }

    let response = create_client_without_request_logging()
        .get(GITHUB_LATEST_RELEASE_URL)
        .header(USER_AGENT, get_codex_user_agent())
        .timeout(REMOTE_CONTROL_LATEST_RELEASE_TIMEOUT)
        .send()
        .await
        .map_err(|err| {
            io::Error::other(format!(
                "failed to fetch latest Codex release metadata: {err}"
            ))
        })?;
    let status = response.status();
    let body = response.bytes().await.map_err(|err| {
        io::Error::other(format!(
            "failed to read latest Codex release metadata: {err}"
        ))
    })?;
    if !status.is_success() {
        return Err(io::Error::other(format!(
            "latest Codex release metadata probe failed: HTTP {status}"
        )));
    }
    let release = serde_json::from_slice::<ReleaseInfo>(&body).map_err(|err| {
        io::Error::other(format!(
            "failed to parse latest Codex release metadata: {err}"
        ))
    })?;
    extract_release_version(&release.tag_name)
}

#[cfg(test)]
async fn latest_stable_release_version() -> io::Result<String> {
    extract_release_version(TEST_LATEST_STABLE_RELEASE_TAG)
}

fn extract_release_version(tag_name: &str) -> io::Result<String> {
    tag_name
        .strip_prefix("rust-v")
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(format!("failed to parse latest release tag `{tag_name}`")))
}

#[cfg(test)]
pub(super) fn expected_reported_app_server_version_for_tests() -> String {
    if env!("CARGO_PKG_VERSION") == DEV_BUILD_VERSION_SENTINEL {
        TEST_DESKTOP_APP_SERVER_VERSION.to_string()
    } else {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::remote_control::auth::RemoteControlConnectionAuth;
    use crate::transport::remote_control::protocol::normalize_remote_control_url;
    use crate::transport::remote_control::server_api::enroll_remote_control_server;
    use codex_state::StateRuntime;
    use codex_utils_absolute_path::test_support::PathExt;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::io::AsyncBufReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;
    use tokio::net::TcpListener;
    use tokio::net::TcpStream;
    use tokio::time::Duration;
    use tokio::time::timeout;

    async fn remote_control_state_runtime(codex_home: &TempDir) -> Arc<StateRuntime> {
        StateRuntime::init(
            codex_state::SqliteConfig::new_for_testing(codex_home.path().abs()),
            "test-provider".to_string(),
        )
        .await
        .expect("state runtime should initialize")
    }

    #[test]
    fn preview_remote_control_response_body_redacts_server_token() {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&preview_remote_control_response_body(
                br#"{"server_id":"srv_e_test","remote_control_token":"secret","pairing_code":"pairing-code","manual_pairing_code":"ABCD-EFGH"}"#
            ))
            .expect("redacted response preview should stay valid json"),
            json!({
                "server_id": "srv_e_test",
                "remote_control_token": "<redacted>",
                "pairing_code": "<redacted>",
                "manual_pairing_code": "<redacted>",
            })
        );
    }

    #[test]
    fn extract_release_version_accepts_rust_prefix() {
        assert_eq!(
            extract_release_version("rust-v0.141.0").expect("release tag should parse"),
            "0.141.0"
        );
    }

    #[test]
    fn extract_desktop_app_bundle_version_accepts_plutil_output() {
        assert_eq!(
            extract_desktop_app_bundle_version("26.616.41845\n")
                .expect("bundle version output should parse"),
            "26.616.41845"
        );
    }

    #[tokio::test]
    async fn reported_app_server_version_prefers_desktop_app_version_for_source_builds() {
        let version = reported_app_server_version().await;

        if env!("CARGO_PKG_VERSION") == DEV_BUILD_VERSION_SENTINEL {
            assert_eq!(version, TEST_DESKTOP_APP_SERVER_VERSION);
        } else {
            assert_eq!(version, env!("CARGO_PKG_VERSION"));
        }
    }

    #[tokio::test]
    async fn remote_control_user_agent_uses_reported_version_for_source_builds() {
        let user_agent = remote_control_user_agent().await;
        let expected_version = expected_reported_app_server_version_for_tests();

        assert!(user_agent.contains(&format!("/{expected_version} ")));
        if env!("CARGO_PKG_VERSION") == DEV_BUILD_VERSION_SENTINEL {
            assert!(user_agent.starts_with("Codex Desktop/"));
            assert_eq!(remote_control_originator(), "Codex Desktop");
        }
    }

    #[tokio::test]
    async fn cached_remote_control_user_agent_matches_resolved_version() {
        let user_agent = remote_control_user_agent().await;

        assert_eq!(cached_remote_control_user_agent(), Some(user_agent));
    }

    #[tokio::test]
    async fn persisted_remote_control_enrollment_round_trips_by_target_and_account() {
        let codex_home = TempDir::new().expect("temp dir should create");
        let state_db = remote_control_state_runtime(&codex_home).await;
        let first_target = normalize_remote_control_url("https://chatgpt.com/remote/control")
            .expect("first target should parse");
        let second_target =
            normalize_remote_control_url("https://api.chatgpt-staging.com/other/control")
                .expect("second target should parse");
        let first_enrollment = RemoteControlEnrollment {
            remote_control_target: first_target.clone(),
            account_id: "account-a".to_string(),
            environment_id: "env_first".to_string(),
            server_id: "srv_e_first".to_string(),
            server_name: "first-server".to_string(),
            remote_control_token: None,
            expires_at: None,
            next_refresh_at: None,
        };
        let second_enrollment = RemoteControlEnrollment {
            remote_control_target: second_target.clone(),
            account_id: "account-a".to_string(),
            environment_id: "env_second".to_string(),
            server_id: "srv_e_second".to_string(),
            server_name: "second-server".to_string(),
            remote_control_token: None,
            expires_at: None,
            next_refresh_at: None,
        };

        update_persisted_remote_control_enrollment(
            Some(state_db.as_ref()),
            &first_target,
            "account-a",
            Some("desktop-client"),
            Some(&first_enrollment),
            /*remote_control_enabled*/ None,
        )
        .await
        .expect("first enrollment should persist");
        update_persisted_remote_control_enrollment(
            Some(state_db.as_ref()),
            &second_target,
            "account-a",
            Some("desktop-client"),
            Some(&second_enrollment),
            /*remote_control_enabled*/ None,
        )
        .await
        .expect("second enrollment should persist");

        assert_eq!(
            load_persisted_remote_control_enrollment(
                Some(state_db.as_ref()),
                &first_target,
                "account-a",
                Some("desktop-client"),
            )
            .await
            .expect("first enrollment should load"),
            Some(first_enrollment.clone())
        );
        assert_eq!(
            load_persisted_remote_control_enrollment(
                Some(state_db.as_ref()),
                &first_target,
                "account-b",
                Some("desktop-client"),
            )
            .await
            .expect("missing account should load"),
            None
        );
        assert_eq!(
            load_persisted_remote_control_enrollment(
                Some(state_db.as_ref()),
                &second_target,
                "account-a",
                Some("desktop-client"),
            )
            .await
            .expect("second enrollment should load"),
            Some(second_enrollment)
        );
    }

    #[tokio::test]
    async fn clearing_persisted_remote_control_enrollment_removes_only_matching_entry() {
        let codex_home = TempDir::new().expect("temp dir should create");
        let state_db = remote_control_state_runtime(&codex_home).await;
        let first_target = normalize_remote_control_url("https://chatgpt.com/remote/control")
            .expect("first target should parse");
        let second_target =
            normalize_remote_control_url("https://api.chatgpt-staging.com/other/control")
                .expect("second target should parse");
        let first_enrollment = RemoteControlEnrollment {
            remote_control_target: first_target.clone(),
            account_id: "account-a".to_string(),
            environment_id: "env_first".to_string(),
            server_id: "srv_e_first".to_string(),
            server_name: "first-server".to_string(),
            remote_control_token: None,
            expires_at: None,
            next_refresh_at: None,
        };
        let second_enrollment = RemoteControlEnrollment {
            remote_control_target: second_target.clone(),
            account_id: "account-a".to_string(),
            environment_id: "env_second".to_string(),
            server_id: "srv_e_second".to_string(),
            server_name: "second-server".to_string(),
            remote_control_token: None,
            expires_at: None,
            next_refresh_at: None,
        };

        update_persisted_remote_control_enrollment(
            Some(state_db.as_ref()),
            &first_target,
            "account-a",
            /*app_server_client_name*/ None,
            Some(&first_enrollment),
            /*remote_control_enabled*/ None,
        )
        .await
        .expect("first enrollment should persist");
        update_persisted_remote_control_enrollment(
            Some(state_db.as_ref()),
            &second_target,
            "account-a",
            /*app_server_client_name*/ None,
            Some(&second_enrollment),
            /*remote_control_enabled*/ None,
        )
        .await
        .expect("second enrollment should persist");

        update_persisted_remote_control_enrollment(
            Some(state_db.as_ref()),
            &first_target,
            "account-a",
            /*app_server_client_name*/ None,
            /*enrollment*/ None,
            /*remote_control_enabled*/ None,
        )
        .await
        .expect("matching enrollment should clear");

        assert_eq!(
            load_persisted_remote_control_enrollment(
                Some(state_db.as_ref()),
                &first_target,
                "account-a",
                /*app_server_client_name*/ None,
            )
            .await
            .expect("cleared enrollment should load"),
            None
        );
        assert_eq!(
            load_persisted_remote_control_enrollment(
                Some(state_db.as_ref()),
                &second_target,
                "account-a",
                /*app_server_client_name*/ None,
            )
            .await
            .expect("remaining enrollment should load"),
            Some(second_enrollment)
        );
    }

    #[tokio::test]
    async fn enroll_remote_control_server_parse_failure_includes_response_body() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let remote_control_url = format!(
            "http://127.0.0.1:{}/backend-api/",
            listener
                .local_addr()
                .expect("listener should have a local addr")
                .port()
        );
        let remote_control_target =
            normalize_remote_control_url(&remote_control_url).expect("target should parse");
        let enroll_url = remote_control_target.enroll_url.clone();
        let response_body = json!({
            "server_id": "srv_e_test",
            "environment_id": "env_test",
        });
        let expected_body = response_body.to_string();
        let server_task = tokio::spawn(async move {
            let stream = accept_http_request(&listener).await;
            respond_with_json(stream, response_body).await;
        });

        let err = enroll_remote_control_server(
            &remote_control_target,
            &RemoteControlConnectionAuth {
                auth_provider: codex_model_provider::unauthenticated_auth_provider(),
                account_id: "account_id".to_string(),
            },
            "11111111-1111-4111-8111-111111111111",
            "test-server",
        )
        .await
        .expect_err("invalid response should fail to parse");

        server_task.await.expect("server task should succeed");
        assert_eq!(
            err.to_string(),
            format!(
                "failed to parse remote control server enrollment response from `{enroll_url}`: HTTP 200 OK, request-id: <none>, cf-ray: <none>, body: {expected_body}, decode error: missing field `remote_control_token` at line 1 column {}",
                expected_body.len()
            )
        );
    }

    async fn accept_http_request(listener: &TcpListener) -> TcpStream {
        let (stream, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("HTTP request should arrive in time")
            .expect("listener accept should succeed");
        let mut reader = BufReader::new(stream);

        let mut request_line = String::new();
        reader
            .read_line(&mut request_line)
            .await
            .expect("request line should read");
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .await
                .expect("header line should read");
            if line == "\r\n" {
                break;
            }
        }

        reader.into_inner()
    }

    async fn respond_with_json(mut stream: TcpStream, body: serde_json::Value) {
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .await
            .expect("response should write");
        stream.flush().await.expect("response should flush");
    }
}
