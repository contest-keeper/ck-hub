//! Where a request came from, so a limit can count one source apart from the others. The global
//! caps stay as they were: they protect the database and the hashing, this only stops one noisy
//! source from using them up for everyone.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::HeaderMap;
use axum::middleware::Next;
use axum::response::Response;

use crate::hub::Hub;

/// Failed logins one source may make in [`LOGIN_WINDOW_SECONDS`].
pub(crate) const MAX_LOGIN_FAILURES: u32 = 10;
pub(crate) const LOGIN_WINDOW_SECONDS: i64 = 300;
/// Failed enrolments one source may make in a minute.
pub(crate) const MAX_ENROL_FAILURES: u32 = 10;
pub(crate) const ENROL_WINDOW_SECONDS: i64 = 60;
/// Forge codes and pairings one source may start in a minute.
pub(crate) const MAX_FORGE_STARTS: u32 = 10;
pub(crate) const FORGE_WINDOW_SECONDS: i64 = 60;

/// The address a request is counted under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source(pub String);

/// The rightmost `X-Forwarded-For` entry is the address the Azure Container Apps ingress appended,
/// so a client cannot choose it; the entries before it are whatever the client sent. Without the
/// header the address of the socket is used.
pub(crate) fn source_of(headers: &HeaderMap, peer: Option<SocketAddr>) -> Source {
    let forwarded = headers
        .get_all("x-forwarded-for")
        .iter()
        .next_back()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|entry| !entry.is_empty());
    let text = forwarded.map_or_else(
        || peer.map_or_else(|| "unknown".to_owned(), |peer| peer.ip().to_string()),
        str::to_owned,
    );
    // A bounded key: an address is never longer than this.
    Source(text.chars().take(64).collect())
}

/// What the proxy in front of the Hub adds to a request it passes on. They count only with the
/// secret the two share: anyone can send a header.
pub(crate) const INGRESS_SECRET: &str = "x-ck-ingress-secret";
pub(crate) const CLIENT_ADDRESS: &str = "x-ck-client-ip";
pub(crate) const PUBLIC_HOST: &str = "x-ck-forwarded-host";

/// When the proxy sent the request, takes the client's address and the public host name from it.
/// Without that every client would count as one source, and a browser's `Origin` would not match.
fn behind_proxy(headers: &mut HeaderMap, secret: Option<&str>) -> Option<Source> {
    let given = headers.remove(INGRESS_SECRET);
    let client = headers.remove(CLIENT_ADDRESS);
    let host = headers.remove(PUBLIC_HOST);
    // Compared as hashes, so the time taken says nothing about the secret.
    let hash = |bytes: &[u8]| ck_crypto::sha256_hex(bytes);
    if hash(given?.as_bytes()) != hash(secret?.as_bytes()) {
        return None;
    }
    if let Some(host) = host {
        headers.insert("x-forwarded-host", host);
    }
    let client = client?.to_str().ok()?.chars().take(64).collect();
    Some(Source(client))
}

/// Puts the [`Source`] of the request where the handlers can read it.
pub async fn layer(State(hub): State<Arc<Hub>>, mut request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let source = behind_proxy(request.headers_mut(), hub.ingress_secret.as_deref())
        .unwrap_or_else(|| source_of(request.headers(), peer));
    request.extensions_mut().insert(source);
    next.run(request).await
}

impl Hub {
    /// Whether `source` has used up its failures for `scope`.
    pub(crate) fn source_blocked(
        &self,
        scope: &str,
        source: &Source,
        max: u32,
        window: i64,
    ) -> bool {
        !self.accounts.under_limit(
            &format!("src-{scope}:{}", source.0),
            (self.clock)(),
            max,
            window,
        )
    }

    /// Counts a failure of `source` for `scope`.
    pub(crate) fn source_failed(&self, scope: &str, source: &Source, window: i64) {
        self.accounts
            .count(&format!("src-{scope}:{}", source.0), (self.clock)(), window);
    }

    /// Counts a start by `source` for `scope`; false once it is over `max`.
    pub(crate) fn source_take(&self, scope: &str, source: &Source, max: u32, window: i64) -> bool {
        self.accounts.count_if_under(
            &format!("src-{scope}:{}", source.0),
            (self.clock)(),
            max,
            window,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for value in values {
            map.append("x-forwarded-for", value.parse().expect("header"));
        }
        map
    }

    #[test]
    fn the_proxys_headers_count_only_with_its_secret() {
        let proxied = |secret: &str| {
            let mut map = HeaderMap::new();
            for (name, value) in [
                (INGRESS_SECRET, secret),
                (CLIENT_ADDRESS, "9.9.9.9"),
                (PUBLIC_HOST, "hub.example.org"),
            ] {
                map.insert(name, value.parse().expect("header"));
            }
            map
        };
        let mut good = proxied("s3cret");
        assert_eq!(
            behind_proxy(&mut good, Some("s3cret")),
            Some(Source("9.9.9.9".into()))
        );
        assert_eq!(good["x-forwarded-host"], "hub.example.org");
        assert!(!good.contains_key(INGRESS_SECRET));
        for (given, kept) in [("guess", Some("s3cret")), ("s3cret", None)] {
            let mut map = proxied(given);
            assert_eq!(behind_proxy(&mut map, kept), None);
            assert!(map.is_empty());
        }
    }

    #[test]
    fn the_rightmost_forwarded_address_is_the_source() {
        let peer = Some("10.0.0.9:1".parse().expect("addr"));
        assert_eq!(
            source_of(&headers(&["6.6.6.6, 1.2.3.4"]), peer),
            Source("1.2.3.4".into())
        );
        assert_eq!(
            source_of(&headers(&["6.6.6.6", "1.2.3.4"]), peer),
            Source("1.2.3.4".into())
        );
        assert_eq!(source_of(&headers(&[]), peer), Source("10.0.0.9".into()));
        assert_eq!(source_of(&headers(&[]), None), Source("unknown".into()));
    }
}
