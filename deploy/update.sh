#!/usr/bin/env bash
# Pull, build, and restart kamaji on the VM. Run this over SSH as a sudo-capable
# user (not as kamaji itself — kamaji has no login shell, see README Deployment).
#
# Usage: sudo bash /opt/kamaji/src/deploy/update.sh
set -euo pipefail

SRC_DIR=/opt/kamaji/src
BIN_DIR=/opt/kamaji/bin
SERVICE=kamaji

if [[ $EUID -ne 0 ]]; then
  echo "must run as root (sudo bash $0)" >&2
  exit 1
fi

echo "==> pulling latest source"
sudo -u kamaji git -C "$SRC_DIR" pull

echo "==> building release binaries (kamajid daemon + kamaji CLI)"
sudo -u kamaji sh -c ". \"\$HOME/.cargo/env\" && cd $SRC_DIR && cargo build --release --workspace"

echo "==> installing binaries"
install -o kamaji -g kamaji -m 755 "$SRC_DIR/target/release/kamajid" "$BIN_DIR/kamajid"
install -o kamaji -g kamaji -m 755 "$SRC_DIR/target/release/kamaji" "$BIN_DIR/kamaji"

echo "==> restarting service"
systemctl restart "$SERVICE"

echo "==> status"
systemctl status "$SERVICE" --no-pager -l
