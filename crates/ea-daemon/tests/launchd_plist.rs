//! What `scripts/install-launchd.sh` writes into the plist.
//!
//! The installer is shell, so nothing else in this workspace type-checks it,
//! and the two keys asserted here are the ones whose absence is silent: a job
//! that crash-loops, and a job that crash-loops *fast*. Both only show up
//! weeks later as a multi-gigabyte log nobody was watching.

fn installer() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/install-launchd.sh")
        .canonicalize()
        .expect("the installer script must exist");
    std::fs::read_to_string(path).expect("reading the installer script")
}

/// Pull `<key>Name</key><integer>N</integer>` out of the heredoc.
fn integer_key(source: &str, key: &str) -> Option<u64> {
    let marker = format!("<key>{key}</key>");
    let rest = &source[source.find(&marker)? + marker.len()..];
    let open = rest.find("<integer>")? + "<integer>".len();
    let close = rest.find("</integer>")?;
    rest[open..close].trim().parse().ok()
}

/// `KeepAlive` plus a fatal startup error is a crash loop, and the log it
/// writes to is rotated by a job inside the daemon that never finishes
/// starting. launchd's own default throttle is 10 seconds, which is not a
/// back-off; the plist has to name a longer one explicitly.
#[test]
fn the_plist_throttles_a_crash_loop_well_past_the_launchd_default() {
    let source = installer();
    assert!(
        source.contains("<key>KeepAlive</key>"),
        "the premise of this test: the job is restarted when it dies"
    );

    let throttle = integer_key(&source, "ThrottleInterval")
        .expect("the plist must set ThrottleInterval explicitly");
    assert!(
        throttle >= 60,
        "ThrottleInterval is {throttle}s; launchd's default of 10s means a fatal \
         startup error restarts the daemon six times a minute, forever, into a \
         log only the daemon itself can rotate"
    );
}

/// The daemon rotates these two itself, in the retention job, so their paths
/// must be the ones it knows about — the state directory.
#[test]
fn the_plist_points_both_logs_at_the_state_directory() {
    let source = installer();
    for key in ["StandardOutPath", "StandardErrorPath"] {
        assert!(
            source.contains(&format!("<key>{key}</key>")),
            "the plist must set {key}"
        );
    }
    assert!(source.contains("$LOG_DIR/daemon.out.log"), "{source}");
    assert!(source.contains("$LOG_DIR/daemon.err.log"), "{source}");
}
