//! Conventions every outbound HTTP request in this workspace shares.
//!
//! There is exactly one of these so far, and it earned its place the hard
//! way. `reqwest` sends no `User-Agent` header at all unless you ask it to,
//! and on 2026-09-25 the Canvas connector's first call against the real
//! `canvas.kth.se` came back:
//!
//! ```text
//! GET https://canvas.kth.se/api/v1/courses -> 403 Forbidden
//! "You are not authorized to access this site because you have not
//!  provided a valid user agent."
//! ```
//!
//! Confirmed in both directions with `curl` against that host: `-A ""` gives
//! 403, `-A "exec-agent/0.1"` gives 200. Every test in this workspace talks
//! to `wiremock` on loopback, which does not care, so nothing caught it.
//!
//! The other vendors accept an anonymous client today, but several ask for an
//! identifying `User-Agent` in their API guidelines and any of them could
//! start enforcing it the way Canvas does. Identifying the client is the
//! correct thing to do regardless, so all of them send it.

/// The `User-Agent` every HTTP client in this workspace sends.
///
/// The version comes from `ea-core`'s own crate metadata rather than a
/// literal, so a `cargo` version bump carries through instead of rotting.
/// Every crate in the workspace is versioned in lockstep, so `ea-core`'s
/// version is the product's version.
pub const USER_AGENT: &str = concat!("exec-agent/", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_agent_names_the_product_and_a_version() {
        assert_eq!(
            USER_AGENT,
            format!("exec-agent/{}", env!("CARGO_PKG_VERSION"))
        );
    }

    /// An empty or malformed `User-Agent` is what the 403 was about, so pin
    /// the shape: a product token, a slash, and a non-empty version.
    #[test]
    fn the_user_agent_is_a_product_token_and_a_non_empty_version() {
        let (product, version) = USER_AGENT
            .split_once('/')
            .expect("a `product/version` user agent");
        assert_eq!(product, "exec-agent");
        assert!(!version.is_empty(), "{USER_AGENT:?} carries no version");
        assert!(
            version.starts_with(|c: char| c.is_ascii_digit()),
            "{USER_AGENT:?} does not look like a version"
        );
    }

    /// `reqwest` rejects a header value with a control character or a
    /// non-ASCII byte at build time, which would turn every client
    /// constructor in the workspace into a start-up failure.
    #[test]
    fn the_user_agent_is_a_legal_header_value() {
        assert!(
            USER_AGENT
                .bytes()
                .all(|b| b.is_ascii_graphic() || b == b' '),
            "{USER_AGENT:?} is not a legal HTTP header value"
        );
    }
}
