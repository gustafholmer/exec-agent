//! Test doubles shared by the tool modules.
//!
//! `ea-fortnox`'s own `auth::test_support` is `#[cfg(test)] pub(crate)` and so
//! is invisible from here; these are the same two doubles, minimal versions.
//!
//! Every server built here points at a `wiremock::MockServer` on loopback with
//! a token that never expires and a refresh backend that panics if it is ever
//! called. **No test in this crate can reach api.fortnox.se**: the base URL is
//! the mock's, the store is in memory, and a real refresh would need a network
//! the backend refuses to attempt.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use ea_fortnox::auth::{
    FortnoxTokenResponse, RefreshBackend, StoredTokens, TokenManager, TokenStore,
};
use ea_fortnox::errors::FortnoxError;
use ea_fortnox::FortnoxClient;
use wiremock::{MockServer, ResponseTemplate};

use super::write::LineArg;
use super::FortnoxServer;

/// A token that will not expire during the test run.
const LIVE_TOKEN: &str = "test-access-token";

fn far_future() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2099, 1, 1, 0, 0, 0)
        .single()
        .expect("a real timestamp")
}

/// One set of tokens, held in memory and never written anywhere.
struct FixedStore;

impl TokenStore for FixedStore {
    fn load(&self) -> anyhow::Result<Option<StoredTokens>> {
        Ok(Some(StoredTokens {
            access_token: LIVE_TOKEN.to_string(),
            refresh_token: "test-refresh-token".to_string(),
            expires_at: far_future(),
            scope: "bookkeeping".to_string(),
        }))
    }

    fn save(&self, _tokens: &StoredTokens) -> anyhow::Result<()> {
        Ok(())
    }
}

/// A refresh backend that must never be reached. If a test trips it, the
/// stored token was treated as stale — which would mean the test was about to
/// talk to Fortnox's real token endpoint.
struct NeverRefresh;

impl RefreshBackend for NeverRefresh {
    fn refresh<'a>(
        &'a self,
        _refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<FortnoxTokenResponse, FortnoxError>> + Send + 'a>> {
        Box::pin(async {
            panic!("a test tried to refresh a Fortnox token; the stored one must stay live")
        })
    }
}

/// A server whose client points at `mock`.
pub(crate) fn server_for(mock: &MockServer) -> FortnoxServer {
    let tokens = Arc::new(TokenManager::new(
        Arc::new(FixedStore),
        Arc::new(NeverRefresh),
    ));
    FortnoxServer::new(
        FortnoxClient::with_base_url(tokens, &mock.uri()).expect("a client against the mock"),
    )
}

/// A server with no credentials at all. The previews still work on one.
pub(crate) fn unconfigured() -> FortnoxServer {
    FortnoxServer::unconfigured("no Fortnox integration at /nonexistent/fortnox/app.json")
}

/// `set_body_raw`: wiremock's `set_body_string` stamps `text/plain` over any
/// content type set before it, and the client rejects a non-JSON reply.
pub(crate) fn json_body(body: serde_json::Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
}

/// One voucher line, as a tool argument.
pub(crate) fn line(account: &str, debit: f64, credit: f64, info: Option<&str>) -> LineArg {
    LineArg {
        account: account.to_string(),
        debit,
        credit,
        info: info.map(str::to_string),
    }
}
