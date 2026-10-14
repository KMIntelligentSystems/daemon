//! Capability lockdown for the oracle child process — defense-in-depth below
//! the closed tool catalog.
//!
//! The trusted Rust airlock owns the Azure identity. In Azure Container Apps it
//! uses the platform-provided managed-identity endpoint to mint one short-lived
//! Foundry token. The untrusted Node oracle receives only that token and the
//! validated Foundry project endpoint; it never receives IDENTITY_ENDPOINT,
//! IDENTITY_HEADER, client secrets, data-source keys, or the HMAC key.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::process::Command;

const FOUNDRY_RESOURCE: &str = "https://ai.azure.com";
const MANAGED_IDENTITY_API_VERSION: &str = "2019-08-01";

#[derive(Debug, Deserialize)]
struct ManagedIdentityTokenResponse {
    access_token: String,
    #[serde(default)]
    expires_on: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClientCredentialTokenResponse {
    access_token: String,
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Acquire a Foundry-only token from Azure Container Apps' local managed-
/// identity endpoint. Omitting an identity selector chooses the Container
/// App's system-assigned identity. A user-assigned identity can be selected
/// explicitly with FOUNDRY_MANAGED_IDENTITY_CLIENT_ID.
fn managed_identity_foundry_token() -> Result<String> {
    let endpoint = std::env::var("IDENTITY_ENDPOINT")
        .context("IDENTITY_ENDPOINT is unavailable; managed identity is not configured")?;
    let identity_header = std::env::var("IDENTITY_HEADER")
        .context("IDENTITY_HEADER is unavailable; managed identity is not configured")?;

    let mut url = url::Url::parse(&endpoint).context("parsing IDENTITY_ENDPOINT")?;
    match url.scheme() {
        "http" | "https" => {}
        scheme => bail!("IDENTITY_ENDPOINT uses unsupported scheme '{scheme}'"),
    }
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("resource", FOUNDRY_RESOURCE);
        query.append_pair("api-version", MANAGED_IDENTITY_API_VERSION);
        if let Ok(client_id) = std::env::var("FOUNDRY_MANAGED_IDENTITY_CLIENT_ID") {
            if !client_id.trim().is_empty() {
                query.append_pair("client_id", client_id.trim());
            }
        }
    }
/*
 GET ${IDENTITY_ENDPOINT}?resource=https%3A%2F%2Fai.azure.com&api-version=2019-08-01
   X-IDENTITY-HEADER: ${IDENTITY_HEADER}
*/
    let response = ureq::get(url.as_str())
        .set("X-IDENTITY-HEADER", &identity_header)
        .call()
        .context("requesting Foundry token from ACA managed-identity endpoint")?;
    let token_response: ManagedIdentityTokenResponse = response
        .into_json()
        .context("parsing ACA managed-identity token response")?;

    if token_response.access_token.trim().is_empty() {
        bail!("ACA managed-identity endpoint returned an empty access token");
    }
    if let Some(token_type) = token_response.token_type.as_deref() {
        if !token_type.eq_ignore_ascii_case("Bearer") {
            bail!("ACA managed-identity endpoint returned unexpected token type");
        }
    }
    if let Some(resource) = token_response.resource.as_deref() {
        if resource.trim_end_matches('/') != FOUNDRY_RESOURCE {
            bail!("ACA managed-identity endpoint returned unexpected resource '{resource}'");
        }
    }

    eprintln!(
        "[airlock] Foundry auth: ACA managed identity{}",
        token_response
            .expires_on
            .as_deref()
            .map(|expiry| format!(" (expires_on={expiry})"))
            .unwrap_or_default()
    );
    Ok(token_response.access_token)
}

/// Local-development fallback using the old Daemon app registration. This is
/// disabled unless ALLOW_CLIENT_SECRET_AUTH=true and must not be enabled in ACA.
fn client_credentials_foundry_token() -> Result<String> {
    if std::env::var("ALLOW_CLIENT_SECRET_AUTH").as_deref() != Ok("true") {
        bail!("client-secret authentication is disabled");
    }
    let tenant = std::env::var("AZURE_TENANT_ID").context("AZURE_TENANT_ID is required")?;
    let client_id = std::env::var("AZURE_CLIENT_ID").context("AZURE_CLIENT_ID is required")?;
    let secret = std::env::var("AZURE_CLIENT_SECRET").context("AZURE_CLIENT_SECRET is required")?;
    let body = format!(
        "grant_type=client_credentials&client_id={}&client_secret={}&scope={}",
        percent_encode(&client_id),
        percent_encode(&secret),
        percent_encode("https://ai.azure.com/.default"),
    );
    let url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let response = ureq::post(&url)
        .set("Content-Type", "application/x-www-form-urlencoded")
        .send_string(&body)
        .context("requesting local-development Foundry token from Entra")?;
    let token_response: ClientCredentialTokenResponse = response
        .into_json()
        .context("parsing client-credential token response")?;
    if token_response.access_token.trim().is_empty() {
        bail!("Entra returned an empty Foundry access token");
    }
    eprintln!("[airlock] Foundry auth: explicit local client-secret fallback");
    Ok(token_response.access_token)
}

/// Credential precedence:
/// 1. Explicit short-lived token (tests/trusted parent injection).
/// 2. ACA managed identity (production).
/// 3. Client secret only when explicitly enabled for local development.
fn foundry_token() -> Result<String> {
    if let Ok(token) = std::env::var("FOUNDRY_ACCESS_TOKEN") {
        if !token.trim().is_empty() {
            eprintln!("[airlock] Foundry auth: supplied short-lived token");
            return Ok(token);
        }
    }
    if std::env::var_os("IDENTITY_ENDPOINT").is_some()
        || std::env::var_os("IDENTITY_HEADER").is_some()
    {
        return managed_identity_foundry_token();
    }
    client_credentials_foundry_token().context(
        "no Foundry credential available: configure ACA managed identity or explicitly enable the local client-secret fallback",
    )
}

fn validate_foundry_endpoint(raw: &str) -> Result<String> {
    let endpoint = url::Url::parse(raw).context("parsing AZURE_AI_PROJECT_ENDPOINT")?;
    if endpoint.scheme() != "https" {
        bail!("AZURE_AI_PROJECT_ENDPOINT must use https");
    }
    if !endpoint.username().is_empty() || endpoint.password().is_some() {
        bail!("AZURE_AI_PROJECT_ENDPOINT must not contain credentials");
    }
    if endpoint.query().is_some() || endpoint.fragment().is_some() {
        bail!("AZURE_AI_PROJECT_ENDPOINT must not contain query or fragment components");
    }
    let host = endpoint
        .host_str()
        .context("AZURE_AI_PROJECT_ENDPOINT has no host")?;
    if !host.eq_ignore_ascii_case("localhost")
        && !host.eq_ignore_ascii_case("127.0.0.1")
        && !host.eq_ignore_ascii_case("::1")
        && !host.ends_with(".services.ai.azure.com")
    {
        bail!("AZURE_AI_PROJECT_ENDPOINT host is not an allowed Foundry host");
    }
    if !endpoint.path().starts_with("/api/projects/") {
        bail!("AZURE_AI_PROJECT_ENDPOINT must be a Foundry project endpoint");
    }
    Ok(endpoint.as_str().trim_end_matches('/').to_string())
}

/// Prepare a Command to spawn the oracle with the minimum possible environment.
/// Token acquisition happens in the trusted parent before env_clear().
pub fn harden_child(cmd: &mut Command) -> Result<()> {
    let token = foundry_token()?;
    let endpoint = validate_foundry_endpoint(
        &std::env::var("AZURE_AI_PROJECT_ENDPOINT")
            .context("AZURE_AI_PROJECT_ENDPOINT is required")?,
    )?;

    let preserve = if cfg!(target_os = "windows") {
        vec!["SystemRoot", "PATH", "USERPROFILE", "TMP", "TEMP"]
    } else {
        vec!["PATH", "HOME"]
    };
    let mut new_env = std::collections::HashMap::<String, String>::new();
    for key in &preserve {
        if let Ok(value) = std::env::var(key) {
            new_env.insert(key.to_string(), value);
        }
    }
    new_env.insert("FOUNDRY_ACCESS_TOKEN".to_string(), token);
    new_env.insert("AZURE_AI_PROJECT_ENDPOINT".to_string(), endpoint);

    cmd.env_clear();
    for (key, value) in &new_env {
        cmd.env(key, value);
    }

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        // Apply limits in the forked child immediately before exec. Applying
        // them here in the parent would also constrain the trusted airlock.
        unsafe {
            cmd.pre_exec(|| {
                let limits = [
                    (libc::RLIMIT_RSS, 512 * 1024 * 1024),
                    (libc::RLIMIT_CPU, 60),
                    (libc::RLIMIT_NPROC, 1),
                    (libc::RLIMIT_NOFILE, 16),
                ];
                for (resource, value) in limits {
                    let limit = libc::rlimit {
                        rlim_cur: value,
                        rlim_max: value,
                    };
                    if libc::setrlimit(resource, &limit) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }

    // An empty-directory chroot before exec cannot load Node, its dynamic
    // linker, CA roots, or the oracle script. Leave filesystem isolation to a
    // purpose-built runtime root/container policy rather than pretending a
    // best-effort chroot protects the child.

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_form_values() {
        assert_eq!(percent_encode("a b/+"), "a%20b%2F%2B");
    }

    #[test]
    fn accepts_expected_foundry_endpoint() {
        let endpoint = validate_foundry_endpoint(
            "https://forecastingmodule.services.ai.azure.com/api/projects/proj-default/",
        )
        .unwrap();
        assert_eq!(
            endpoint,
            "https://forecastingmodule.services.ai.azure.com/api/projects/proj-default"
        );
    }

    #[test]
    fn rejects_non_foundry_endpoint() {
        assert!(
            validate_foundry_endpoint("https://evil.example/api/projects/proj-default").is_err()
        );
        assert!(validate_foundry_endpoint(
            "http://forecastingmodule.services.ai.azure.com/api/projects/proj-default"
        )
        .is_err());
        assert!(validate_foundry_endpoint(
            "https://forecastingmodule.services.ai.azure.com/not-a-project"
        )
        .is_err());
    }
}
