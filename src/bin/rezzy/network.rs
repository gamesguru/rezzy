// Copyright 2026 Shane Jaroch
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// ureq's `native-tls` cargo feature only makes the `native-tls` crate
// available as a dependency; it does not wire it up as the TLS backend for
// ureq's default agent (that only happens automatically with the
// rustls-backed `tls` feature). Without explicitly building an agent with a
// TLS connector, `ureq::get()` has no TLS backend and every https:// request
// fails with "Unknown Scheme".
#[cfg(feature = "tls-native")]
fn build_agent() -> anyhow::Result<ureq::Agent> {
    let connector = native_tls::TlsConnector::new()
        .map_err(|e| anyhow::anyhow!("Failed to initialize native-tls backend: {e}"))?;
    Ok(ureq::AgentBuilder::new()
        .tls_connector(std::sync::Arc::new(connector))
        .build())
}

// With only `tls-rustls` enabled, ureq's own `tls` feature (rustls) is on
// and its default agent already comes with a working TLS backend.
#[cfg(all(feature = "tls-rustls", not(feature = "tls-native")))]
fn build_agent() -> anyhow::Result<ureq::Agent> {
    Ok(ureq::AgentBuilder::new().build())
}

#[cfg(not(any(feature = "tls-native", feature = "tls-rustls")))]
fn build_agent() -> anyhow::Result<ureq::Agent> {
    anyhow::bail!(
        "rezzy was built without a TLS backend: rebuild with `--features tls-native` \
         (system OpenSSL) or `--features tls-rustls` (pure-Rust, no system deps)"
    )
}

pub fn fetch_room_state(
    homeserver: &str,
    room_id: &str,
    token: Option<&str>,
) -> anyhow::Result<serde_json::Value> {
    let base = if homeserver.starts_with("http://") || homeserver.starts_with("https://") {
        homeserver.to_string()
    } else {
        format!("https://{homeserver}")
    };
    let url = format!("{base}/_matrix/client/v3/rooms/{room_id}/state");
    eprintln!("Fetching {url}");
    let agent = build_agent()?;
    let mut request = agent.get(&url);
    if let Some(t) = token {
        request = request.set("Authorization", &format!("Bearer {t}"));
    }

    let response = match request.call() {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            anyhow::bail!("HTTP {code}: {body}");
        }
        Err(e) => anyhow::bail!("Request failed: {e}"),
    };
    let body = response.into_string()?;

    let val: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse JSON: {}. Response: {}",
            e,
            &body[..body.len().min(500)]
        )
    })?;
    Ok(val)
}
