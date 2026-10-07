//! `phoebis tls reload`: one `POST /admin/tls/reload` through the admin client, printed as one line per public
//! listener or as the route's body.

use std::time::Duration;

use hyper::Method;
use serde_json::Value;

use crate::admin_client::{AdminClient, ClientError, Common};

#[derive(clap::Args, Debug)]
pub struct TlsReloadArgs {
    #[command(flatten)]
    pub common: Common,
}

/// One POST through the admin client under `--timeout`, the route's body.
pub async fn read(client: &AdminClient, deadline: Duration) -> Result<Value, ClientError> {
    match tokio::time::timeout(
        deadline,
        client.expect(Method::POST, "/admin/tls/reload", None),
    )
    .await
    {
        Ok(answer) => answer.map(|answer| answer.body),
        Err(_) => Err(ClientError::Transport {
            url: client.url(),
            reason: format!("the call did not finish within {deadline:?}"),
        }),
    }
}

/// One line per listener: `proxy: reloaded, certificate {path} expires {date}`, `proxy: not reloaded, {the loader's
/// line}. The router keeps the pair it was serving.`, or `proxy: plain HTTP, nothing to reload`.
pub fn render(body: &Value) -> String {
    listeners(body)
        .iter()
        .map(|entry| {
            let listener = entry["listener"].as_str().unwrap_or("listener");
            match (entry["serving"].as_str(), entry["reloaded"].as_bool()) {
                (Some("plain"), _) => format!("{listener}: plain HTTP, nothing to reload\n"),
                (_, Some(true)) => format!(
                    "{listener}: reloaded, certificate {} expires {}\n",
                    entry["cert_file"].as_str().unwrap_or_default(),
                    entry["expires"].as_str().unwrap_or_default()
                ),
                _ => format!(
                    "{listener}: not reloaded, {}. The router keeps the pair it was serving.\n",
                    entry["error"]
                        .as_str()
                        .unwrap_or("the router named no reason")
                ),
            }
        })
        .collect()
}

/// 0 when every pair reloaded (a plain listener does not count), 1 when any refused. A body that names no listener
/// list confirms no reload, so it answers 1 too.
pub fn exit_code(body: &Value) -> i32 {
    let Some(entries) = body["listeners"].as_array() else {
        return 1;
    };
    let reloaded = entries
        .iter()
        .all(|entry| entry["serving"] == "plain" || entry["reloaded"] == true);
    if reloaded {
        0
    } else {
        1
    }
}

fn listeners(body: &Value) -> &[Value] {
    body["listeners"].as_array().map_or(&[], Vec::as_slice)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn each_listener_prints_its_own_line_and_a_refused_pair_alone_sets_the_exit_code() {
        let body = json!({"listeners": [
            {"listener": "proxy", "serving": "tls", "reloaded": true, "cert_file": "/etc/phoebis/proxy.pem",
             "expires": "2027-10-05T12:00:00Z", "days_left": 364},
            {"listener": "admin", "serving": "plain"},
        ]});
        assert_eq!(
            render(&body),
            "proxy: reloaded, certificate /etc/phoebis/proxy.pem expires 2027-10-05T12:00:00Z\n\
             admin: plain HTTP, nothing to reload\n"
        );
        assert_eq!(exit_code(&body), 0);

        let refused = json!({"listeners": [
            {"listener": "proxy", "serving": "tls", "reloaded": false,
             "error": "proxy.tls.cert_file /etc/phoebis/proxy.pem: holds no PEM certificate"},
            {"listener": "admin", "serving": "plain"},
        ]});
        assert_eq!(
            render(&refused),
            "proxy: not reloaded, proxy.tls.cert_file /etc/phoebis/proxy.pem: holds no PEM certificate. The router \
             keeps the pair it was serving.\n\
             admin: plain HTTP, nothing to reload\n"
        );
        assert_eq!(exit_code(&refused), 1);
        assert_eq!(exit_code(&json!({"listeners": []})), 0);
        assert_eq!(exit_code(&json!({"error": "not found"})), 1);
    }
}
