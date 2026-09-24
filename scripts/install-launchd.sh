#!/bin/bash
#
# Build exec-agent and install it as a launchd user agent.
#
# Idempotent: run it again after any change and it rebuilds, rewrites the
# plist and reloads the job.
#
#   ./scripts/install-launchd.sh            # build, install, load
#   ./scripts/install-launchd.sh --uninstall
#
set -euo pipefail

LABEL="dev.gustaf.exec-agent"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="${EA_STATE_DIR:-$HOME/.local/state/exec-agent}"

if [[ "${1:-}" == "--uninstall" ]]; then
  launchctl unload "$PLIST" 2>/dev/null || true
  rm -f "$PLIST"
  echo "Unloaded and removed $PLIST"
  exit 0
fi

# Homebrew's rustc is broken on this machine; rustup's is not. Prefer it.
export PATH="$HOME/.cargo/bin:$PATH"

echo "==> Building (release)"
cd "$REPO"
cargo build --release

DAEMON="$REPO/target/release/ea-daemon"
[[ -x "$DAEMON" ]] || { echo "error: $DAEMON was not built" >&2; exit 1; }

# The daemon shells out to `claude`, and launchd hands a job a minimal
# environment -- typically just /usr/bin:/bin:/usr/sbin:/sbin, with none of the
# shell profile that puts `claude` on an interactive PATH. So the plist carries
# an explicit PATH, and the directory holding `claude` is found *here*, in a
# login shell, rather than guessed.
CLAUDE_BIN="$(command -v claude || true)"
if [[ -z "$CLAUDE_BIN" ]]; then
  echo "error: \`claude\` is not on PATH." >&2
  echo "       The daemon shells out to it for triage and chat; install it," >&2
  echo "       or re-run this script from a shell where \`command -v claude\` works." >&2
  exit 1
fi
CLAUDE_DIR="$(cd "$(dirname "$CLAUDE_BIN")" && pwd)"
echo "==> Found claude at $CLAUDE_BIN"

# The release binaries live next to ea-daemon and are resolved relative to it,
# so target/release goes on the PATH too. /usr/bin and friends stay for the
# CLI's own subprocesses.
JOB_PATH="$CLAUDE_DIR:$REPO/target/release:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"

mkdir -p "$LOG_DIR" "$HOME/Library/LaunchAgents"
chmod 700 "$LOG_DIR"

echo "==> Writing $PLIST"
cat > "$PLIST" <<PLIST_EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>$LABEL</string>

  <key>ProgramArguments</key>
  <array>
    <string>$DAEMON</string>
  </array>

  <!-- Connectors are discovered under ./connectors, so the daemon must run
       from the repository. -->
  <key>WorkingDirectory</key>
  <string>$REPO</string>

  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>

  <key>EnvironmentVariables</key>
  <dict>
    <key>PATH</key>
    <string>$JOB_PATH</string>
    <key>HOME</key>
    <string>$HOME</string>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>

  <key>StandardOutPath</key>
  <string>$LOG_DIR/daemon.out.log</string>
  <key>StandardErrorPath</key>
  <string>$LOG_DIR/daemon.err.log</string>

  <!-- A crash loop should back off rather than spin. -->
  <key>ThrottleInterval</key>
  <integer>10</integer>
</dict>
</plist>
PLIST_EOF

echo "==> Reloading"
launchctl unload "$PLIST" 2>/dev/null || true
launchctl load "$PLIST"

echo
echo "Installed. Check it with:"
echo "  launchctl list | grep $LABEL"
echo "  $REPO/target/release/ea status"
echo "  tail -f $LOG_DIR/daemon.err.log"
