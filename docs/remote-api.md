# Remote `kamaji` CLI over the REST API

A guide to running `kamaji` commands (`ingest`, `fact`, `todo`, `status`, `history`) from a second machine over the internet, instead of only from the Hetzner VM itself over the local Unix socket.

This is **opt-in**: leaving `REST_API_BIND` unset means `kamajid` never binds an HTTP listener at all — behavior is identical to a build with none of this code. Reachability is plain public HTTPS on its own subdomain, fronted by Caddy (already running on this VM for Tuwunel — see `docs/matrix.md`) for automatic Let's Encrypt TLS; auth is a TOTP code (like Google Authenticator) exchanged for a bearer session token. A private overlay (Tailscale/WireGuard) was considered and intentionally not used, to keep one fewer service to maintain on a solo VM.

---

## 0. Set your variables

```bash
export KAMAJI_DOMAIN="kamaji.pasoenfalso.com"   # your chosen subdomain, alongside matrix.pasoenfalso.com
export KAMAJI_REST_PORT="8081"                   # loopback-only port kamajid binds; anything unused is fine
```

These two are shell-session convenience variables for following this guide only — `kamajid` itself never reads `KAMAJI_DOMAIN` or `KAMAJI_REST_PORT`. They only exist so the commands below can be copy-pasted as-is; each step that uses them (the unit override in step 2, the Caddyfile in step 3, the `curl`s in step 4) has the shell substitute the actual value in at the point the command runs, so what actually lands in `/etc/systemd/system/kamaji.service.d/override.conf` is a literal `Environment="REST_API_BIND=127.0.0.1:8081"`, not a reference to `$KAMAJI_REST_PORT` — don't add `KAMAJI_DOMAIN`/`KAMAJI_REST_PORT` themselves to the systemd unit. Like `docs/matrix.md`'s `$MATRIX_DOMAIN`, these only last for your current shell session — re-export them if you reconnect partway through.

**Before continuing:** add an **A record** (and **AAAA** if you have IPv6) for `$KAMAJI_DOMAIN` pointing at this VM's public IP. Ports 80/443 are already open on this VM for Caddy (Matrix uses them already) — nothing new to open at the firewall.

---

## 1. Enroll a TOTP secret

On the VM, before starting (or restarting) `kamajid`:

```bash
sudo -u kamaji /opt/kamaji/bin/kamajid --print-totp-setup
```

This prints an `otpauth://` URI, a terminal QR code, and the raw base32 secret — scan the QR (or type the secret manually) into your authenticator app now. This is the only time the secret is shown; it isn't stored anywhere until you set the env var in step 2.

---

## 2. Configure `kamajid`

Of the three new env vars, only `REST_API_TOTP_SECRET` is a secret — same split `docs/hardening.md` §1.4 already uses for `TELEGRAM_BOT_TOKEN`/`MATRIX_ACCESS_TOKEN`: secrets go in the root-only `EnvironmentFile`, everything else stays as plain, visible `Environment=` lines in the unit override.

Add the secret to `/etc/kamaji/secrets.env` (already root:root, mode 600 per hardening.md):

```bash
sudo tee -a /etc/kamaji/secrets.env > /dev/null <<'EOF'
REST_API_TOTP_SECRET=<the base32 secret from step 1>
EOF
```

(Replace `<the base32 secret from step 1>` with the actual value before running — the quoted `'EOF'` here just disables shell expansion inside the heredoc as a general precaution, matching the README's convention for secrets, even though base32 doesn't happen to use `$`.)

Add the rest to the unit override (same file the README's step 6 already creates at `/etc/systemd/system/kamaji.service.d/override.conf`):

```bash
sudo tee -a /etc/systemd/system/kamaji.service.d/override.conf > /dev/null <<EOF
Environment="REST_API_BIND=127.0.0.1:${KAMAJI_REST_PORT}"
# Optional, defaults to 7 days:
# Environment="REST_API_SESSION_TTL_SECS=604800"
EOF
```

`kamajid` only ever binds this to `127.0.0.1` — it never speaks TLS and is never directly reachable from the internet. Caddy (step 3) is the only public-facing listener.

You've just edited a `.service.d/override.conf` drop-in, so `daemon-reload` has to come before `restart` — otherwise systemd re-execs the process with the environment it already had cached, and the new `Environment=` lines silently don't take effect (you'd see `transport::socket: listening for cli connections` in `journalctl -u kamaji` but no matching `transport::rest` line):

```bash
sudo systemctl daemon-reload
sudo systemctl restart kamaji
```

Confirm the REST listener actually came up:

```bash
journalctl -u kamaji -n 20 --no-pager | grep transport::rest
# expect: "listening for rest api connections bind_addr=127.0.0.1:8081"
```

---

## 3. Add a Caddy site block

Caddy is already installed and running on this VM. Append a second site block to the existing `/etc/caddy/Caddyfile` (don't overwrite it — the Tuwunel block from `docs/matrix.md` needs to stay):

```
${KAMAJI_DOMAIN} {
    reverse_proxy 127.0.0.1:${KAMAJI_REST_PORT}
}
```

Validate and reload:

```bash
sudo caddy validate --config /etc/caddy/Caddyfile
sudo systemctl reload caddy
```

Caddy issues and renews the Let's Encrypt cert for the new block automatically, the same way it already does for the Matrix one.

---

## 4. Verify from the VM

```bash
curl -s -X POST https://${KAMAJI_DOMAIN}/auth/login -H "Content-Type: application/json" -d '{"code":"000000"}'
# expect: {"error":"invalid totp code"} with HTTP 401
```

The `-H "Content-Type: application/json"` is required — `curl -d` defaults to `application/x-www-form-urlencoded`, and axum's JSON extractor correctly rejects that with `415 Unsupported Media Type` rather than guessing at the body's format.

A `401` here (rather than a connection error or a TLS failure) confirms Caddy, the cert, and `kamajid`'s REST listener are all wired up correctly. Try again with a real code from your authenticator app to confirm a `200` with a token.

---

## 5. Set up the remote machine

Install the `kamaji` binary on the second machine (build it there, or copy the binary — it has no runtime dependency on the VM beyond network access).

```bash
kamaji --remote https://${KAMAJI_DOMAIN} login
# prompts for the current code from your authenticator app,
# caches the session token at ~/.kamaji/token (mode 0600)

kamaji --remote https://${KAMAJI_DOMAIN} status
kamaji --remote https://${KAMAJI_DOMAIN} ingest "https://example.com/some-article"
```

Set `KAMAJI_REMOTE_URL` in your shell profile to avoid repeating `--remote` on every call. An explicitly typed `--socket <path>` or `--local` still forces the local socket for a single call even with `KAMAJI_REMOTE_URL` set — see [Transport resolution order](../README.md#transport-resolution-order) in the README for the full precedence between `--remote`, `--socket`, `--local`, and their env fallbacks.

If a command fails with a session-expired message, just re-run `kamaji --remote https://${KAMAJI_DOMAIN} login` — sessions last `REST_API_SESSION_TTL_SECS` (7 days by default).

---

## 6. Logging out / revoking access

Normal logout, from the machine that has the cached token:

```bash
kamaji --remote https://${KAMAJI_DOMAIN} logout
```

This clears `~/.kamaji/token` locally and asks `kamajid` to delete that session server-side. It clears the local cache even if the daemon can't be reached — logging out is always a local guarantee first, the server-side revoke is best-effort on top.

If the device holding the token is lost, stolen, or otherwise not something you can run `logout` from, revoke every outstanding session at once, from the VM:

```bash
kamajid --revoke-all-sessions
```

This is a one-off operation directly against the redb database (only needs `REDB_PATH`, the same default as everything else) — it doesn't need `kamajid` to be running, and doesn't touch any of the daemon's other env vars. Every device with a cached token will need to `kamaji login` again afterward.

---

## Notes

- `/auth/login` is rate-limited per source IP (a handful of attempts per minute) — this only matters because the endpoint is reachable from the public internet; it wouldn't be necessary behind a private overlay, but plain HTTPS needs it to keep a captured/guessed TOTP attempt from being brute-forced.
- Each TOTP code is single-use even within its own clock-skew window — `kamajid` tracks the last accepted time step in memory (reset on restart), so a captured code can't be replayed.
- A REST-originated job shows up in logs/`job_history` tagged distinctly from a local socket connection (`ChatRef::Rest` vs `ChatRef::Cli`) — useful if you ever need to tell which transport a given ingest/fact came through.
- There's no way to single out and revoke one specific token from the daemon side without the token itself — that's why the default TTL is short (7 days) and why `kamajid --revoke-all-sessions` is all-or-nothing rather than per-device.
