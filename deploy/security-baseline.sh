#!/usr/bin/env bash
# Read-only security baseline / audit for the Kamaji VM.
#
# Prints a point-in-time forensic snapshot: successful & failed logins, signs of
# post-compromise activity, persistence mechanisms, listening sockets, and kamaji
# app health. Nothing here writes state or changes config -- safe to run anytime.
#
# Usage (run over SSH on the VM, needs sudo for the journal & other users' files):
#   sudo bash /opt/kamaji/src/deploy/security-baseline.sh
#   sudo bash /opt/kamaji/src/deploy/security-baseline.sh | tee ~/baseline-$(date +%F).txt
#
# A saved report contains source IPs and usernames -- chmod 600 it if you keep it.
#
# Compare the output against the "Baseline (2026-07-22)" section of docs/hardening.md:
# the known-good key fingerprint, source IPs, expected listeners, and noise floor.
# The lines that matter most: any `Accepted password` (should be none), any key or
# UID-0 account you don't recognize, any listener that isn't sshd/caddy/tuwunel.
set -uo pipefail   # no -e: a missing log source should skip a section, not abort

if [[ $EUID -ne 0 ]]; then
  echo "must run as root for full visibility (sudo bash $0)" >&2
  exit 1
fi

echo "════════ KAMAJI VM SECURITY BASELINE — $(date -u) ════════"

echo; echo "── 1. SUCCESSFUL LOGINS (who got in) ──"
last -aiF | head -30
echo "-- Accepted auths from journal --"
journalctl -u sshd 2>/dev/null | grep -iE "Accepted (password|publickey)" | tail -20

echo; echo "── 2. FAILED-LOGIN BASELINE (scan noise floor) ──"
FAILS=$(journalctl -u sshd 2>/dev/null | grep -icE "Failed password|Invalid user|authentication failure")
echo "total failed attempts in journal: $FAILS"
echo "-- top 15 source IPs --"
journalctl -u sshd 2>/dev/null | grep -iE "Failed password|Invalid user" | grep -oE "from [0-9.]+" | sort | uniq -c | sort -rn | head -15
echo "-- top guessed usernames --"
journalctl -u sshd 2>/dev/null | grep -oE "Invalid user [a-z0-9_-]+" | sort | uniq -c | sort -rn | head -10

echo; echo "── 3. POST-COMPROMISE SIGNS ──"
echo "-- sudo commands (last 20) --"
journalctl _COMM=sudo 2>/dev/null | grep COMMAND | tail -20
echo "-- accounts with a real login shell --"
grep -vE "/(nologin|false|sync)$" /etc/passwd
echo "-- UID 0 accounts (expect only root) --"
awk -F: '$3==0' /etc/passwd
echo "-- authorized_keys across all users (unexpected key = backdoor) --"
find /home /root -name authorized_keys -exec echo "  file: {}" \; -exec cat {} \; 2>/dev/null

echo; echo "── 4. PERSISTENCE (cron / timers) ──"
ls -la /etc/cron.d/ /var/spool/cron/ 2>/dev/null
for u in $(awk -F: '$3>=1000 || $1=="root" || $1=="kamaji" {print $1}' /etc/passwd); do
  C=$(crontab -l -u "$u" 2>/dev/null); [ -n "$C" ] && { echo "  crontab for $u:"; echo "$C"; }
done
echo "-- active timers --"
systemctl list-timers --all --no-pager 2>/dev/null | head -15

echo; echo "── 5. LISTENING SOCKETS (expect sshd, +caddy/tuwunel if Matrix) ──"
ss -tulpnH 2>/dev/null | sort
echo "-- firewall (expect inbound 22 + 80/443 if Matrix, nothing else) --"
firewall-cmd --list-all 2>/dev/null || echo "firewalld not present -- check the Hetzner Cloud Firewall in the console"

echo; echo "── 6. KAMAJI APP HEALTH ──"
echo "-- restarts / exits (crash-loop or probing) --"
journalctl -u kamaji 2>/dev/null | grep -iE "started|stopped|failed|main process exited" | tail -20
echo "-- ingest/parse failures --"
journalctl -u kamaji 2>/dev/null | grep -iE "parse|failed|error|dropped|not allowed" | tail -20
echo "-- processes owned by kamaji (expect kamajid + transient claude/git) --"
ps -u kamaji -f 2>/dev/null

echo; echo "════════ END BASELINE ════════"
