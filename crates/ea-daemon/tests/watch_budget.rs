//! The poll budget has to fit inside the poll interval.
//!
//! [`ea_daemon::jobs::WATCH_TIMEOUT`] bounds one `watch_poll`; each
//! connector's `connector.toml` says how often that poll is due. If the two
//! are equal — as they were, at 120 seconds each, for the Google connector —
//! a poll that uses its whole budget finishes exactly as the next one falls
//! due, so a merely slow connector runs polls back to back forever. The
//! scheduler's overlap guard then skips the ticks it collides with, which
//! silently halves the polling rate rather than reporting anything.
//!
//! This test reads the manifests in the repository rather than a fixture,
//! because the mistake it guards against is made in a manifest.

use std::time::Duration;

use ea_daemon::jobs::WATCH_TIMEOUT;

fn connectors_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../connectors")
        .canonicalize()
        .expect("the connectors directory must exist")
}

#[test]
fn no_connector_polls_faster_than_a_poll_is_allowed_to_take() {
    let mut checked = 0;

    for entry in std::fs::read_dir(connectors_dir()).expect("reading connectors/") {
        let manifest = entry
            .expect("a directory entry")
            .path()
            .join("connector.toml");
        if !manifest.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&manifest).expect("reading a connector manifest");
        let parsed: toml::Value = text.parse().expect("a connector manifest must be TOML");
        let Some(interval) = parsed
            .get("watch_interval_secs")
            .and_then(toml::Value::as_integer)
        else {
            continue;
        };
        checked += 1;

        let interval = Duration::from_secs(interval as u64);
        assert!(
            WATCH_TIMEOUT < interval,
            "{} polls every {interval:?} but one poll may take {WATCH_TIMEOUT:?}: a slow poll \
             would run back to back forever, and the scheduler's overlap guard would hide it \
             by skipping ticks instead of reporting a timeout",
            manifest.display()
        );
    }

    assert!(
        checked > 0,
        "this test asserts nothing unless it found a manifest declaring watch_interval_secs"
    );
}
