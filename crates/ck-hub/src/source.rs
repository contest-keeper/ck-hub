//! Where a request came from, so a limit can count one source apart from the others. The global
//! caps stay as they were: they protect the database and the hashing, this only stops one noisy
//! source from using them up for everyone.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request};
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
fn source_of(headers: &HeaderMap, peer: Option<SocketAddr>) -> Source {
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

/// Puts the [`Source`] of the request where the handlers can read it.
pub async fn layer(mut request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let source = source_of(request.headers(), peer);
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
