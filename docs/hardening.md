# Hardening the Kamaji VM

Practical hardening plan for the single Hetzner VM that runs `kamajid` under systemd (see the Deployment section of `README.md`), and optionally Tuwunel + Caddy (see `docs/matrix.md`).

Each item has a **priority** (what it buys you) and a **complexity** (what it costs you). Work top-down: the list is ordered so that the first section is the highest security-per-minute, and the last is genuine-but-optional depth.

Two things shape this whole document and are worth stating once:

- **The VM holds three credentials that matter more than the box itself**: the Telegram bot token, a read-write GitHub PAT for `cvicens/notes`, and (if Matrix is on) a Matrix access token. An attacker who reads `/home/kamaji/.git-credentials-notes` doesn't need to keep the VM — they already have what's valuable. Prioritize *credential* containment over generic host hardening.
- **`kamajid` shells out to `claude` and `git`.** Any sandboxing you apply to the unit applies to those children too. That rules out a few otherwise-standard systemd directives, called out inline below.

---

## Baseline (2026-07-22)

A point-in-time forensic snapshot taken *before* any hardening, so future alerting has a documented "normal" to diff against. Re-run `deploy/security-baseline.sh` periodically and compare. **Verdict at capture: clean — no evidence of compromise, past or present.**

**Known-good facts (an alert should fire when reality stops matching these):**

- **Authorized key — the only credential in use.** Exactly one key authorizes SSH, present in both `claude` and `root` `authorized_keys` (the `root` copy carries the stock Fedora cloud-image `command="…login as NONE…";exit 142` forced-command lockout, so it grants no root shell):
  - Fingerprint: `<your-key-fingerprint>` (ED25519) — recorded privately, not in this repo.
  - Comment: `<your-key-comment>` — **confirmed the owner's own key**. Any *second* key appearing, or this one changing, is a red flag.
- **Auth method:** publickey only. Zero successful password logins, ever. If a `Accepted password` line ever appears, investigate.
- **Known-good source IPs:** tracked privately (not in this repo, since they identify the owner's location) — see your own baseline capture. A *successful* login from anything outside that list warrants a look — failed attempts from elsewhere are just noise (below).
- **Accounts:** one UID-0 (`root`), one real login shell (`claude`, UID 1000). No user crontabs; only stock Fedora timers.
- **Listeners:** `sshd` on `:22`; Caddy on `:80`/`:443`; Tuwunel bound **localhost-only** (`127.0.0.1:8008`) behind Caddy; `8448` closed (non-federating, as intended). Anything new bound to `0.0.0.0` on a fresh port is the signal to chase.
- **Daemon:** single `kamajid` process, no restart loop.

**Scan noise floor:** ~61,000 failed SSH attempts in the journal at capture — pure background scanning (`admin`, `ubuntu`, `solana`/`sol`, all `Invalid user` against nonexistent accounts). This is the volume to *subtract out*; alert on deviations from it, never on the raw count.

**Findings carried forward from this baseline (not compromise — hardening backlog):**

1. **SSRF is reachable and proven.** The ingest worker was observed fetching `http://127.0.0.1:4317` (its own localhost OTLP port) from message-driven content. Swap that for `169.254.169.254` and it's the Hetzner metadata endpoint. → implement the fetcher deny-list in §3.2.
2. **Firewall unverified at capture.** Listeners look correct, but firewalld/Hetzner Cloud Firewall rules weren't confirmed. Expected inbound: `22`, `80`, `443`, nothing else. → §1.2.
3. **Telegram network errors** clustered on the capture day (`Connection reset by peer` on `GetUpdates`); daemon recovered each time via its watchdog (no restart). Network degradation, not security — but exactly the state-change class the future `kamaji notify` should surface.
4. **LLMNR exposed** (`systemd-resolved` on `0.0.0.0:5355`) — harmless behind the firewall, needless surface. Set `LLMNR=no` in `/etc/systemd/resolved.conf` if tidying.

---

## Tier 1 — Do these first (high impact, low complexity)

### 1.1 Lock down SSH

**Priority: critical · Complexity: low (15 min)**

The single largest exposed surface on a public-IP VM.

**Current state: key-based login on port 22.** The important half is already done — but logging in with a key does not mean password auth is *disabled*. `sshd` accepts both simultaneously, and most distro defaults still permit passwords. So this section is a verification task first and a config change second.

#### Step 1: check what sshd actually resolves to

```bash
sudo sshd -T | grep -iE 'passwordauthentication|permitrootlogin|kbdinteractive|pubkeyauth|maxauthtries'
```

Use `sshd -T`, not `grep` over `/etc/ssh/sshd_config`. `-T` prints the *effective* config after all `Include` directives and built-in defaults are resolved, so it reflects what the daemon will actually do. Reading the main config file alone routinely gives the wrong answer, because a drop-in under `sshd_config.d/` (Fedora and Ubuntu both ship them) can override what you see there, and unset directives fall back to compiled-in defaults that appear nowhere in any file.

What you want to see:

```
passwordauthentication no
kbdinteractiveauthentication no
permitrootlogin no
pubkeyauthentication yes
```

If `passwordauthentication` comes back `yes`, that's the one genuine gap: every brute-force bot scanning port 22 has a live target, and your key is irrelevant to them. Fix it below.

`kbdinteractiveauthentication` is the one people miss. On many builds it's a second route to password auth through PAM, so setting only `PasswordAuthentication no` can leave the door open. Both need to be `no`.

#### Step 2: close whatever the check found

```bash
sudo tee /etc/ssh/sshd_config.d/99-hardening.conf > /dev/null <<'EOF'
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin no
PubkeyAuthentication yes
MaxAuthTries 3
LoginGraceTime 20
AllowUsers <your-login-user>
EOF

sudo sshd -t && sudo systemctl reload sshd
sudo sshd -T | grep -iE 'passwordauthentication|kbdinteractive'   # confirm it took
```

The `99-` prefix matters: drop-ins are read in lexical order and, in OpenSSH, **the first value wins** for most keywords. A distro-shipped `50-redhat.conf` setting `PasswordAuthentication yes` would beat a `10-hardening.conf`, so a high number is not cosmetic. Also confirm the main `sshd_config` has its `Include /etc/ssh/sshd_config.d/*.conf` line near the top rather than the bottom, for the same reason — the re-check in the last line above is what proves it.

`sshd -t` before reload is not optional — a syntax error plus a reload on a remote box is how you lock yourself out. **Keep your current SSH session open** and verify with a second, new session before closing the first. `reload` (not `restart`) also leaves existing sessions untouched.

`AllowUsers` is the highest-value line here given that password auth may already be off: it makes `kamaji` — and every other system account — non-loginable over SSH regardless of any future shell change. The account is created with `-s /bin/false`, so this is defense in depth rather than the only control, but it's the control that survives someone later "fixing" that shell to debug something.

#### Step 3: audit the key itself

```bash
ssh-keygen -l -f ~/.ssh/authorized_keys
```

Two things to confirm: there is **exactly one** key and you recognize it (stale keys from old laptops accumulate silently), and it isn't an `ssh-rsa` key under 3072 bits. If it's an old small RSA key, generate an ed25519 replacement — install the new key and verify it works in a second session *before* removing the old one.

#### On port 22

**Recommendation: leave it.** Moving to a high port stops zero targeted attacks — anyone who matters runs a full port scan — and it costs you a config line in every client, `scp`, and CI job from now on. What it genuinely buys is far quieter auth logs, which matters only if you intend to actually read them and spot anomalies. That's log hygiene, not security, and it ranks below everything else in Tier 1. `fail2ban` (2.3) addresses the same noise without the permanent ergonomic tax.

### 1.2 Firewall: default-deny inbound

**Priority: critical · Complexity: low (10 min)**

Kamaji needs **zero** inbound ports. Telegram is long polling (outbound), git push is outbound, the Claude CLI is outbound, and the CLI client talks over a Unix socket, not TCP. If Matrix is off, the only inbound port on the whole box should be SSH.

```bash
# Fedora / firewalld
sudo firewall-cmd --permanent --remove-service=cockpit   # if present
sudo firewall-cmd --permanent --add-service=ssh
sudo firewall-cmd --reload
sudo firewall-cmd --list-all
```

Do the same at the **Hetzner Cloud Firewall** layer (in the console), not just on the host. That gives you a control plane that survives a misconfigured or disabled host firewall, and it drops traffic before it reaches the VM's NIC.

If Matrix is enabled, add exactly `80/tcp` and `443/tcp` for Caddy. Federation's `8448` stays closed — `docs/matrix.md` already commits to a non-federating homeserver, so keep the firewall enforcing that promise rather than relying on Tuwunel's config alone.

### 1.3 Sandbox the systemd unit

**Priority: high · Complexity: low (20 min, but test carefully)**

`deploy/kamaji.service` currently has no hardening directives at all. The daemon runs as `kamaji`, which is good, but that user can still read most of the filesystem, write anywhere it owns, and load kernel modules' worth of syscalls it will never use.

Add a drop-in so you can roll it back independently of the unit in git:

```bash
sudo tee /etc/systemd/system/kamaji.service.d/hardening.conf > /dev/null <<'EOF'
[Service]
NoNewPrivileges=yes
PrivateTmp=yes
PrivateDevices=yes
ProtectSystem=strict
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectClock=yes
ProtectHostname=yes
ProtectProc=invisible
RestrictSUIDSGID=yes
RestrictRealtime=yes
RestrictNamespaces=yes
LockPersonality=yes
RemoveIPC=yes
CapabilityBoundingSet=
SystemCallArchitectures=native
SystemCallFilter=@system-service
SystemCallErrorNumber=EPERM
RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6

# ProtectSystem=strict makes / read-only, so name every path that must be writable.
ReadWritePaths=/opt/kamaji/data /opt/kamaji/notes /home/kamaji
EOF

sudo systemctl daemon-reload
sudo systemctl restart kamaji
systemd-analyze security kamaji
```

Three deliberate omissions, each of which will break the daemon if you add them:

- **No `MemoryDenyWriteExecute=yes`.** The `claude` CLI is a JIT'd runtime; W^X kills it at startup. This is the single most common way this hardening block breaks a Claude-invoking service.
- **No `ProtectHome=yes`.** `$HOME=/home/kamaji` holds `.git-credentials-*`, git's global config, and the Claude CLI's own state. It must stay readable *and* writable — hence its presence in `ReadWritePaths`.
- **`PrivateNetwork` is impossible.** Outbound to Telegram, GitHub, and Anthropic is the entire job.

After restarting, **exercise the real paths**, not just `systemctl status`: send a plain message (ingest → Claude → note → commit → push), send `/status` via the CLI over the Unix socket, and confirm a push actually lands in `cvicens/notes`. A syscall filter failure typically shows up as a subprocess dying, which the daemon logs and skips — so a green unit status is not evidence that ingest still works.

`systemd-analyze security kamaji` gives you a numeric exposure score; expect to go from ~9.5 ("UNSAFE") to ~2.5 ("OK") with the block above.

### 1.4 Move secrets out of `Environment=` and into a mode-600 file

**Priority: high · Complexity: low (15 min)**

Today the bot token and Matrix access token live in `Environment=` lines in `override.conf`. Two consequences:

- Any local user can run `systemctl show kamaji` and read every `Environment=` value. It does not require root.
- The values land in the journal on certain unit-related messages.

Switch to an `EnvironmentFile` that only root can read:

```bash
sudo tee /etc/kamaji/secrets.env > /dev/null <<'EOF'
TELEGRAM_BOT_TOKEN=123456:ABC-your-actual-token
MATRIX_ACCESS_TOKEN=...
EOF
sudo chmod 600 /etc/kamaji/secrets.env
sudo chown root:root /etc/kamaji/secrets.env
```

```ini
[Service]
EnvironmentFile=/etc/kamaji/secrets.env
```

Non-secret config (`NOTES_REPO_PATH`, `RUST_LOG`, timeouts) can stay as plain `Environment=` lines — keeping them visible is useful. Note the file is read by systemd as root *before* dropping to `kamaji`, so the daemon's own user never needs read access to it. Verify with `systemctl show kamaji | grep -i token` — you should now see nothing.

Consider also updating `deploy/kamaji.service` in this repo to drop its placeholder `Environment="TELEGRAM_BOT_TOKEN=..."` line entirely, so the committed unit never suggests that tokens belong there.

### 1.5 Automatic security updates

**Priority: high · Complexity: low (10 min)**

An unattended VM that nobody logs into for three months is the classic way a patched-years-ago CVE becomes your incident.

```bash
# Fedora
sudo dnf install -y dnf-automatic
sudo sed -i 's/^apply_updates = no/apply_updates = yes/' /etc/dnf/automatic.conf
sudo sed -i 's/^upgrade_type = default/upgrade_type = security/' /etc/dnf/automatic.conf
sudo systemctl enable --now dnf-automatic.timer
```

Pair it with `sudo dnf install -y dnf-plugin-system-upgrade` awareness: security-only auto-updates won't reboot for a new kernel. Either accept that and reboot manually every so often, or add `kernel` awareness via `needs-restarting -r` in a weekly check.

---

## Tier 2 — Meaningful gains, moderate effort

### 2.1 Scope down and rotate the GitHub PATs

**Priority: high · Complexity: low-medium (30 min, needs GitHub UI work)**

The deployment already does the right structural thing — two separate fine-grained PATs, read-only for code and read-write for notes, in separate credential files with no global `credential.helper`. Two improvements on top:

- **Set an expiry** on both PATs (90 days) rather than "no expiration". This converts a silent, permanent compromise into a loud, time-boxed one. The rotation procedure is already documented in the README (`read` + `printf | sudo -u kamaji tee`) and touches exactly one file.
- **Verify the notes PAT is repository-scoped**, not org- or account-wide, and that its permissions are exactly `Contents: read and write` with nothing else — no `workflows`, no `actions`, no metadata beyond the mandatory.

Worth writing the expiry dates down somewhere you actually look, because a silently expired notes PAT surfaces as "push failed after retries" in Telegram rather than as an obvious alert.

### 2.2 Unix socket permissions

**Priority: medium · Complexity: low (10 min)**

`kamajid` binds a Unix socket (`KAMAJI_SOCKET_PATH`, default `./kamaji.sock` relative to `/opt/kamaji`). Anyone who can write to that socket can enqueue `ingest`/`fact`/`todo` jobs — i.e. cause Claude invocations and commits to your notes repo.

Confirm what the socket's mode actually is after a restart:

```bash
sudo ls -l /opt/kamaji/kamaji.sock
```

The default umask usually yields `srwxr-xr-x`, meaning **any local user can connect**. On a single-admin VM this is a small risk, but it's free to close: put the socket in a directory only `kamaji` can traverse.

```ini
[Service]
Environment="KAMAJI_SOCKET_PATH=/opt/kamaji/data/kamaji.sock"
```

```bash
sudo chmod 700 /opt/kamaji/data
```

Directory-mode enforcement is more robust than socket-mode here, because the socket is recreated on every daemon start (`bind_socket` in `kamajid/src/transport/socket.rs` unlinks and rebinds) while the directory's mode persists. Remember to pass `--socket /opt/kamaji/data/kamaji.sock` when running the CLI as another user, or run it via `sudo -u kamaji`.

### 2.3 Intrusion throttling on SSH

**Priority: medium · Complexity: low-medium (20 min)**

With password auth already off (1.1), brute force can't succeed — but `fail2ban` still cuts log volume and blunts key-enumeration scanning.

```bash
sudo dnf install -y fail2ban
sudo tee /etc/fail2ban/jail.d/sshd.local > /dev/null <<'EOF'
[sshd]
enabled = true
backend = systemd
maxretry = 3
findtime = 10m
bantime = 1h
EOF
sudo systemctl enable --now fail2ban
```

Set `bantime` conservatively (1h, not permanent) — you are the most likely person to trip it, and a permanent self-ban on a remote box with no console access is a bad afternoon. Hetzner does provide console access, so it's recoverable, just tedious.

### 2.4 Back up state that isn't in git

**Priority: medium-high · Complexity: medium (1 hour)**

Notes are pushed to GitHub, so they survive the VM dying. Three things do **not**:

- `/opt/kamaji/data/kamaji.redb` — the queue, job history, and dedupe tables. Losing it means replayed updates get re-ingested (duplicate notes) after a restore.
- `/opt/kamaji/data/matrix-store` — matrix-sdk's session and crypto store. Losing it invalidates the device and breaks refresh-token handling.
- `/etc/kamaji/secrets.env` and `/home/kamaji/.git-credentials-*` — recreatable, but only if you still have the tokens elsewhere.

A nightly `systemd` timer that snapshots those to Hetzner object storage (or just `restic` to a second location) is enough. Note that `redb` is a WAL'd embedded DB: copying the file while the daemon is running can capture a torn state. Either stop the service for the duration of the copy (a few seconds of downtime, entirely acceptable for this workload) or snapshot at the filesystem/volume level.

### 2.5 Keep SELinux enforcing

**Priority: medium · Complexity: medium (variable)**

Check first — it may already be fine:

```bash
getenforce
sudo ausearch -m avc -ts recent
```

If it says `Enforcing` and there are no kamaji-related AVC denials, you're done; do nothing. The temptation to set `permissive` arises the first time something breaks — resist it, and instead read the specific denial. Because everything kamaji does lives under `/opt` with a normal service user and no custom ports, SELinux rarely has an opinion about it in practice.

---

## Tier 3 — Depth, once the above is in place

### 3.1 Constrain the notes git repo blast radius

**Priority: medium · Complexity: medium**

The daemon commits and pushes to `cvicens/notes` automatically, driven by content that arrives from Telegram. Two containment ideas worth considering:

- **Push to a dedicated branch** rather than `main`, with `main` advanced by a human or a scheduled merge. This turns "attacker gets bot token, spams the notes repo" from a history-rewrite problem into a discardable-branch problem.
- **Enable branch protection** on `main` in GitHub so the notes PAT literally cannot force-push, even if stolen.

Whether this is worth it depends on how much you'd mind losing/reverting notes history. If the answer is "not much, it's a personal knowledge base", skip it.

### 3.2 Audit what reaches the Claude subprocess

**Priority: medium · Complexity: medium**

The ingest path fetches URLs found in a message, and then follows links found *inside* that content one level deeper (`MAX_FETCHED_TEXT_BYTES`, level-1 capped at 5 URLs). That means **arbitrary remote content is being fetched by your VM and fed into a Claude prompt**, triggered by anyone who can post in an allow-listed chat.

The existing controls are already the important ones — the chat/room allow-list before enqueue, and never using `--dangerously-skip-permissions` (CLAUDE.md makes both non-negotiable). Beyond that:

- Treat the allow-list as a security control, not a convenience filter. Review `ALLOWED_CHAT_IDS` / `ALLOWED_MATRIX_ROOMS` periodically and keep group chats out of it unless you trust every member.
- Consider whether the level-1 link following is worth its risk surface. It's the piece that turns "a link I sent" into "a page I've never seen, chosen by a page I've never seen".
- SSRF is worth a thought: a message containing `http://169.254.169.254/...` would have the VM fetch Hetzner's metadata endpoint and hand the result to Claude. Blocking link-local, loopback, and RFC1918 destinations in the fetcher is a small, self-contained code change and closes it cleanly.

That last one is a code change rather than a VM setting, but it belongs on this list because the VM's network position is what makes it exploitable.

### 3.3 Tighten the Matrix stack (only if enabled)

**Priority: medium (high if enabled) · Complexity: medium**

`docs/matrix.md` already gets the big things right: non-federating, 8448 never opened, Caddy for automatic TLS. Additions:

- **Disable open registration** on Tuwunel and verify it, rather than assuming the default. A public homeserver with open registration on your VM is an open-ended abuse liability.
- **Apply Tier 1.3-style systemd sandboxing to the Tuwunel and Caddy units too.** Same directives, different `ReadWritePaths`.
- **Subscribe to Tuwunel releases** — it's young software directly exposed to the internet on 443, which puts it in a different risk class than everything else on this box. `dnf-automatic` won't update it, since it was installed from a downloaded RPM, not a repo. That gap is easy to forget.

### 3.4 Log retention and review

**Priority: low-medium · Complexity: low**

```bash
sudo tee /etc/systemd/journald.conf.d/retention.conf > /dev/null <<'EOF'
[Journal]
Storage=persistent
SystemMaxUse=500M
MaxRetentionSec=90day
EOF
sudo systemctl restart systemd-journald
```

Persistent journals matter because "when did this start?" is unanswerable after a reboot with volatile storage.

One caution specific to kamaji: `DEBUG=true` writes full prompts *and* replies to `DEBUG_LOG_PATH`. That file will contain the complete text of everything ingested, including anything personal you've sent the bot. Keep `DEBUG` off in production, and if you turn it on to diagnose something, `chmod 600` the log and delete it afterwards.

### 3.5 Consider dropping the build toolchain from the VM

**Priority: low · Complexity: high**

Right now the VM has a full Rust toolchain, git, and the source clone, because `deploy/update.sh` builds in place. That's a meaningful amount of attack surface and (more practically) a lot of RAM/disk for a box whose job is to poll Telegram.

Building elsewhere and shipping only the binary would shrink the VM to `kamajid` + `claude` + systemd. It's listed last because it's the highest-effort item here and buys the least: it changes your deploy workflow, needs a matching glibc target, and the toolchain isn't remotely reachable anyway. Worth it only if you're already inclined to move to CI-built artifacts for other reasons.

---

## Suggested order of execution

| # | Item | Time | Risk of breaking things |
|---|------|------|------------------------|
| 1 | SSH: verify password auth is off (1.1) | 15 min | Low — keep a session open |
| 2 | Firewall, both layers (1.2) | 10 min | Low |
| 3 | Secrets to EnvironmentFile (1.4) | 15 min | Low |
| 4 | Auto security updates (1.5) | 10 min | Low |
| 5 | systemd sandboxing (1.3) | 20 min | **Medium — test ingest end-to-end** |
| 6 | PAT expiry + scope (2.1) | 30 min | Low |
| 7 | Socket permissions (2.2) | 10 min | Low |
| 8 | fail2ban (2.3) | 20 min | Low |
| 9 | Backups (2.4) | 1 hr | Low |
| 10 | Tier 3 as appetite allows | — | — |

Items 1–4 are all "edit a file, reload a service" and can be done in one sitting. Item 5 is the one to do when you have time to watch it — it's the highest-value change in the list and also the only one likely to break the daemon in a way that isn't immediately obvious from `systemctl status`.

## Verifying the result

```bash
systemd-analyze security kamaji     # expect ~2.5 or lower
sudo ss -tlnp                       # expect only sshd (+ caddy if Matrix)
sudo firewall-cmd --list-all
systemctl show kamaji | grep -i -E 'token|password'   # expect nothing
sudo ls -l /opt/kamaji/data /home/kamaji/.git-credentials-*
```

Then the functional check that actually matters: send a plain message and confirm a note lands in `cvicens/notes`, and send `/status` and confirm it replies immediately. Hardening that silently breaks ingestion is worse than no hardening, because the failure mode is a bot that looks alive and quietly files nothing.
