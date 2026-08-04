# Deploying Tuwunel on Fedora 44 with Caddy

A minimal guide to get a [Tuwunel](https://github.com/matrix-construct/tuwunel) Matrix homeserver running on a Fedora 44 VM, fronted by Caddy for automatic Let's Encrypt TLS. This setup is **closed, non-federating**: it's Kamaji's bot account plus occasional remote login (e.g. Element on your phone) — not a homeserver that talks to the wider Matrix network. Federation port 8448 is never opened.

Reference: [Tuwunel Red Hat deployment docs](https://matrix-construct.github.io/tuwunel/deploying/redhat.html) and [Generic deployment guide](https://matrix-construct.github.io/tuwunel/deploying/generic.html).

---

## 0. Set your variables

Paste this block first and fill in your real domain. Every command below reuses these — nothing else to edit by hand.

```bash
export MATRIX_DOMAIN="your.server.name"        # your real domain, e.g. matrix.example.com
export TUWUNEL_VERSION="v1.6.0"                 # check https://github.com/matrix-construct/tuwunel/releases/latest
export CPU_VARIANT="v3"                         # v1/v2/v3 — detected in step 1, default v3 is fine on most modern CPUs
export RPM_FILE="${TUWUNEL_VERSION}-release-all-x86_64-${CPU_VARIANT}-linux-gnu-tuwunel.rpm"
```

> These `export`s only last for your current shell session. If you disconnect and reconnect (e.g. a new SSH session), re-paste this block before continuing — or save it to a file (`~/.tuwunel-vars`) and run `source ~/.tuwunel-vars` each time.

**Before continuing:** create an **A record** (and **AAAA** if you have IPv6) for `$MATRIX_DOMAIN` in your DNS provider (e.g. GoDaddy) pointing at this VM's public IP. GoDaddy is just your DNS host — Let's Encrypt doesn't care who manages DNS, it just needs the record to resolve. Also make sure ports **80** and **443** are reachable from the internet (federation's **8448** is deliberately never opened in this setup).

**Important:** `server_name` can never be changed later without wiping the database, so make sure `$MATRIX_DOMAIN` is the value you actually want to keep.

---

## 1. Install Tuwunel from the RPM

No RPM repo exists yet, so we download the release asset directly.

```bash
# Check which CPU optimization level your VM supports, then set CPU_VARIANT above accordingly
cat /proc/cpuinfo | grep -Po '(avx|sse)[235]' | sort -u | \
  sed 's/avx5/v4/;s/avx2/v3/;s/sse3/v2/;s/sse2/v1/' | sort
```

If the server refuses to start later with an "Illegal Instruction" error, re-export `CPU_VARIANT` to `v2` or `v1`, rebuild `RPM_FILE`, and reinstall.

```bash
curl -LO "https://github.com/matrix-construct/tuwunel/releases/download/${TUWUNEL_VERSION}/${RPM_FILE}"
sudo dnf install -y "./${RPM_FILE}"
```

This installs:
- Binary at `/usr/sbin/tuwunel`
- Default config at `/etc/tuwunel/tuwunel.toml`
- The `tuwunel.service` systemd unit
- A dedicated `tuwunel` system user (via the RPM's postinstall script)

---

## 2. Configure Tuwunel

Set `server_name` and `database_path` directly with `sed` (no manual editing needed):

```bash
sudo sed -i "s|^#server_name *=.*|server_name = \"${MATRIX_DOMAIN}\"|" /etc/tuwunel/tuwunel.toml
sudo sed -i 's|^#database_path *=.*|database_path = "/var/lib/tuwunel"|' /etc/tuwunel/tuwunel.toml
```

> **Note on `ip_source`:** the [upstream docs](https://matrix-construct.github.io/tuwunel/deploying/generic.html) describe an `ip_source` setting (e.g. `rightmost_x_forwarded_for` for reverse-proxy deployments) for spoofing-resistant client IPs. As of the `v1.6.0` release it does not exist in `tuwunel-example.toml` — the docs are ahead of what shipped. Skipping it means Tuwunel sees Caddy's IP for its own rate-limiting/logs rather than the real client IP; for this closed, single-account, non-federating instance that's cosmetic, not a security gap. Re-check the release notes/example config on future upgrades in case it lands later.

Leave `address`/`port` at their defaults (`127.0.0.1` / `8008`) — Tuwunel sits behind Caddy, not the internet directly. No TLS settings are needed in this file since Caddy terminates TLS.

Fix permissions:

```bash
sudo chown -R root:root /etc/tuwunel
sudo chmod -R 755 /etc/tuwunel

sudo mkdir -p /var/lib/tuwunel/
sudo chown -R tuwunel:tuwunel /var/lib/tuwunel/
sudo chmod 700 /var/lib/tuwunel/
```

---

## 3. Open the firewall

Minimal Fedora cloud images (including Hetzner's) often don't ship `firewalld` at all. Check first:

```bash
rpm -q firewalld || sudo dnf install -y firewalld
sudo systemctl enable --now firewalld
```

Then open the ports:

```bash
sudo firewall-cmd --permanent --add-service=http
sudo firewall-cmd --permanent --add-service=https
sudo firewall-cmd --reload
```

No `8448/tcp` here — federation stays off, so that port never needs to be reachable from the internet.

**Note:** if this VM is on Hetzner Cloud, there's a separate, infrastructure-level Hetzner Cloud Firewall (configured in the console, not on the VM) that may also be gating traffic independently of `firewalld`. Confirm ports 80/443 are allowed there too if the in-VM rules alone don't get traffic through.

---

## 4. Install Caddy

Fedora ships Caddy in its default repos, so this is usually enough:

```bash
sudo dnf install -y caddy
```

(If you want the newest upstream build instead, Caddy's own [COPR repo](https://copr.fedorainfracloud.org/coprs/g/caddy/caddy/) is available: `sudo dnf install -y dnf5-plugins && sudo dnf copr enable -y @caddy/caddy && sudo dnf install -y caddy`.)

---

## 5. Configure Caddy as the reverse proxy

```bash
sudo tee /etc/caddy/Caddyfile > /dev/null <<EOF
${MATRIX_DOMAIN} {
    reverse_proxy localhost:8008
}
EOF
```

That single block:
- Serves port 443 for clients (no `:8448` block — federation is off, nothing needs to listen there)
- Automatically requests and renews a Let's Encrypt certificate via the HTTP-01 challenge (this is what needs your DNS A record and port 80 open)
- Sets the correct reverse-proxy headers

Validate and enable:

```bash
sudo caddy validate --config /etc/caddy/Caddyfile
```

---

## 6. SELinux (Fedora enforces this by default)

If Caddy can serve locally but the reverse proxy to Tuwunel fails, check for a boolean-related denial:

```bash
sudo ausearch -m avc -ts recent
```

If it points at a blocked network connection from Caddy, allow web daemons to make outbound connections:

```bash
sudo setsebool -P httpd_can_network_connect on
```

---

## 7. Start everything

```bash
sudo systemctl enable --now tuwunel
sudo systemctl enable --now caddy
```

---

## 8. Verify

```bash
curl "https://${MATRIX_DOMAIN}/_tuwunel/server_version"
```

That's the only check that applies here — federation is off, so there's nothing listening on 8448 and no reason to run it through the [Matrix Federation Tester](https://federationtester.matrix.org/). Try logging in with any [Matrix client](https://matrix.org/ecosystem/clients) (e.g. Element) pointed at `$MATRIX_DOMAIN` to confirm remote access works.

---

## 9. Create the bot's account and access token

Everything above gets the homeserver running — nothing is logged into it yet. This step creates the account Kamaji's `matrix-sdk` client authenticates as. Don't flip on open registration to do this; gate it with a one-time token so only you can create the account.

```bash
sudo sed -i 's|^#\?allow_registration *=.*|allow_registration = true|' /etc/tuwunel/tuwunel.toml
sudo sed -i "s|^#\?registration_token *=.*|registration_token = \"$(openssl rand -hex 32)\"|" /etc/tuwunel/tuwunel.toml
sudo systemctl restart tuwunel
```

> Check the resulting lines in `/etc/tuwunel/tuwunel.toml` before moving on — confirm they landed uncommented and with the values you expect; `sed` will silently no-op if the pattern doesn't match your installed template.

Pull the token back out for the next command, and set the username you want to register:

```bash
REG_TOKEN=$(sudo grep -Po '(?<=^registration_token = ")[^"]+' /etc/tuwunel/tuwunel.toml)
BOT_USERNAME="kamaji"
BOT_PASSWORD=$(openssl rand -base64 24)
```

Registration is a two-step UIA (User-Interactive Auth) exchange per the Matrix spec: the first call comes back `401` with a `session` id, which you then re-POST alongside the completed `auth` block. Sending the token on the first call isn't enough by itself — the `session` id has to round-trip through both calls, so chain them:

```bash
SESSION=$(curl -s -X POST "https://${MATRIX_DOMAIN}/_matrix/client/v3/register" \
  -H "Content-Type: application/json" \
  -d "{\"username\":\"${BOT_USERNAME}\",\"password\":\"${BOT_PASSWORD}\",\"initial_device_display_name\":\"${BOT_USERNAME}-daemon\"}" \
  | grep -Po '(?<="session":")[^"]+')

curl -s -X POST "https://${MATRIX_DOMAIN}/_matrix/client/v3/register" \
  -H "Content-Type: application/json" \
  -d "{\"auth\":{\"type\":\"m.login.registration_token\",\"token\":\"${REG_TOKEN}\",\"session\":\"${SESSION}\"},\"username\":\"${BOT_USERNAME}\",\"password\":\"${BOT_PASSWORD}\",\"initial_device_display_name\":\"${BOT_USERNAME}-daemon\"}"
```

Save the `access_token`, `device_id`, and `user_id` from the final (200) response — that's what the daemon needs.

This same two calls work for registering a personal account too (e.g. for occasional remote Element login) — just re-set `BOT_USERNAME`/`BOT_PASSWORD` to your own values before running them again, while registration is still open.

Once every account you need is registered, close registration back down so the token can't be reused by anyone else who finds it:

```bash
sudo sed -i 's|^allow_registration *=.*|allow_registration = false|' /etc/tuwunel/tuwunel.toml
sudo systemctl restart tuwunel
```

**Token lifetime matters for an unattended daemon.** Tuwunel's `access_token_ttl` defaults to 604800 seconds (7 days) for clients that support refresh. Don't hand-roll a re-login loop for this — `matrix-sdk` implements the OAuth2-style refresh flow natively and persists the session (access + refresh token) to disk, so wire Kamaji's Matrix client up with a session store from the start rather than a bare access token, or it'll silently stop working a week after every restart. Store whatever the initial bootstrap produces the same way you already store the Telegram bot token: outside the repo, loaded via the systemd unit's `EnvironmentFile=`, never committed.

## 10. Optional next steps

- **Calls (TURN)**: see the [TURN guide](https://matrix-construct.github.io/tuwunel/calls/turn.html) if you want audio/video calls to work reliably.
- **Backups**: set `database_backup_path` to enable RocksDB online backups.

## Troubleshooting

- **Cert never issues**: confirm the A record actually resolves to this VM (`dig +short "$MATRIX_DOMAIN"`) and that port 80 is reachable from the public internet (not just your local network) — the HTTP-01 challenge needs it.
- **`bind: address already in use`**: something else (often `httpd`/nginx) already owns port 80/443 — `sudo ss -tlnp | grep -E ':80|:443'` to find it.