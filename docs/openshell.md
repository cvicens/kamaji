# Running Codex on DeepSeek inside an NVIDIA OpenShell Sandbox

This document describes a working setup for running the **Codex** CLI agent inside an
**NVIDIA OpenShell** sandbox, routed to the **DeepSeek** API instead of OpenAI, using
OpenShell's Providers v2 system for descriptive, reusable credential + policy management.

Claims below have been checked against the official OpenShell docs
([docs.nvidia.com/openshell](https://docs.nvidia.com/openshell/latest/home)) and the
DeepSeek API docs ([api-docs.deepseek.com](https://api-docs.deepseek.com)); anything
based on observed behavior rather than documented behavior is marked as such.

## kamaji's own gateway connection (client mTLS)

This section is about a different concern than the rest of this document: not what runs
*inside* the sandbox (Codex/DeepSeek routing, below), but how `kamajid` itself authenticates
*to the gateway* when `agent::Runner::OpenShell` is active. See CLAUDE.md's Stack section for
the full picture; this is just the provisioning step.

By default, a local OpenShell gateway (Docker/Podman/VM, no OIDC issuer) runs with
`--enable-mtls-auth` on — every connection needs a client certificate, `kamajid` included.
`openshell_sdk`'s own `ClientConfig`/`connect()` has no client-cert support (confirmed from
its source: *"mTLS is intentionally out of scope here... handled by `openshell-cli`'s legacy
path"*), so kamaji hand-builds its own `tonic` `Channel` with a client identity
(`agent::connect_mtls`) instead — the same approach `openshell-cli` uses for `openshell
term`. kamaji never mints its own certificate; it reuses the gateway's existing local
single-user identity.

**Provisioning** (manual, out-of-band — kamaji never creates or rotates this):

```bash
# Copy the existing local client identity kamaji can read. Do NOT re-run
# `openshell-gateway generate-certs` to make a "fresh" one for kamaji — it's
# not confirmed idempotent against an existing CA, and could rotate the CA
# out from under every other client (including your own `openshell term`
# session).
sudo install -d -m 0750 -o kamaji -g kamaji /etc/kamaji/openshell-mtls
sudo install -m 0644 -o kamaji -g kamaji \
  ~/.local/state/openshell/tls/ca.crt \
  ~/.local/state/openshell/tls/client/tls.crt \
  /etc/kamaji/openshell-mtls/
sudo install -m 0600 -o kamaji -g kamaji \
  ~/.local/state/openshell/tls/client/tls.key \
  /etc/kamaji/openshell-mtls/
```

Then add to `/etc/systemd/system/kamaji.service.d/override.conf`, alongside the existing
`OPENSHELL_GATEWAY_URL`/`OPENSHELL_SANDBOX_NAME`/`OPENSHELL_READY_TIMEOUT_SECS` lines:

```
Environment="OPENSHELL_MTLS_DIR=/etc/kamaji/openshell-mtls"
```

`OPENSHELL_MTLS_DIR` unset (the default) keeps the anonymous-TLS path — only correct for a
gateway explicitly started with `--enable-mtls-auth=false`. Do not disable the gateway's mTLS
enforcement as a workaround for a missing `OPENSHELL_MTLS_DIR`: the gateway binds
`0.0.0.0` (needed for the Podman sandbox bridge), so that flag drops client-cert auth for
everything reachable at that port, not just kamaji.

Verify with the `#[ignore]`d `openshell_smoke_test_mtls` test in
`kamaji-core/src/agent.rs` (see its doc comment for the env vars it needs) before relying on
a production deploy.

## Using this sandbox as kamaji's agent (`AGENT_FLAVOR=codex`)

The `codex-deepseek-v2` sandbox set up below is also usable as the agent
`kamaji_core::prompt`'s entry points invoke for real ingest/fact/query jobs — not just for
manual `openshell sandbox exec` testing. Set, alongside the existing OpenShell env vars:

```
Environment="AGENT_FLAVOR=codex"
Environment="OPENSHELL_SANDBOX_NAME=codex-deepseek-v2"
```

`CODEX_BIN` defaults to `"codex"` and doesn't need to be set unless the binary lives
somewhere non-standard in the sandbox. kamaji doesn't validate that `OPENSHELL_SANDBOX_NAME`
actually has Codex configured — pointing `AGENT_FLAVOR=codex` at a sandbox without Codex/
DeepSeek set up (e.g. `healthy-lobster`, which is Claude-only) will fail the same way an
unreachable gateway does today, at every `agent::Runner`-mediated call.

Verify with the `#[ignore]`d `codex_smoke_test_agent_query` test in
`kamaji-core/src/prompt.rs` (see its doc comment for the `OPENSHELL_SMOKE_CODEX_*` env vars
it needs) before relying on a production deploy.

## Why this isn't trivial

A few things make this setup non-obvious:

1. **Codex is a supported OpenShell agent**, pre-installed in the base image and
   auto-configured when passed as the trailing command to `sandbox create` (the CLI
   recognizes `claude`, `codex`, and `opencode` as tool names). However, per the
   Supported Agents matrix, Codex has **no default policy coverage** — unlike Claude Code
   (full coverage) — so it needs an explicit network policy declaring its endpoints and
   binaries, or its API calls are denied by the sandbox proxy.
2. **Custom Providers v2 profiles cannot drive `inference.local` routing.** Per the
   Providers v2 roadmap, `inference_capable` is profile metadata only: attaching an
   inference-capable provider does not (yet) create `inference.local` routes, and
   path-based multi-provider routing is not yet wired to profiles. For any
   OpenAI-compatible API such as DeepSeek, the documented path is a provider of
   **type `openai`** with `OPENAI_BASE_URL` pointing at the provider. A custom profile
   (e.g. type `deepseek-inference`) can carry policy, but **cannot** be used as the
   `inference set --provider` target — and you can't name the custom profile `openai`
   either, because built-in profile IDs and legacy aliases are reserved. Hence the
   two-provider split below.
3. **Codex speaks the Responses API** (`wire_api = "responses"`) to custom providers in
   current builds. DeepSeek's API was historically Chat-Completions-only, which made a
   direct Codex → DeepSeek connection 404. **This is no longer true:** DeepSeek now
   supports the Responses API format natively (base_url `https://api.deepseek.com`),
   added explicitly for Codex compatibility, with unsupported parameters silently
   ignored and the `apply_patch` custom tool supported. Two caveats remain:
   - The Responses API currently supports **only `deepseek-v4-flash`**;
     `deepseek-v4-pro` support is planned for early August 2026. Check
     [Models & Pricing](https://api-docs.deepseek.com/quick_start/pricing) for current
     model IDs before choosing the `--model` value.
   - Routing through `inference.local` is therefore no longer needed to fix a protocol
     mismatch — its remaining (documented) value is credential privacy: the router
     strips the caller-supplied key and injects the real one, so the key never reaches
     the *agent process* (it does live in the in-sandbox proxy's memory — see
     `openshell-gw-proxy.md` §4). The OpenShell docs list `POST /v1/responses` among the
     patterns the router accepts and forwards for OpenAI-compatible providers.
4. **The real Codex binary lives at `/usr/bin/codex`** in this image, not
   `/usr/local/bin/codex` — worth confirming with `which codex` inside the sandbox. The
   docs confirm the mechanism: profile `binaries` are the executable paths allowed to
   reach the profile endpoints, so a policy/profile that only lists the wrong path
   silently does nothing.

## Architecture

```
Codex (in sandbox)
  → ~/.codex/config.toml points model_provider at "openshell-deepseek"
  → base_url = https://inference.local/v1
  → OpenShell privacy router (inference.local)
      - strips caller-supplied credentials
      - injects real DeepSeek key from the "deepseek-inference" provider (type: openai)
      - forwards to https://api.deepseek.com/v1
  → DeepSeek API
```

Two separate OpenShell **provider instances** are used, for two different jobs:

| Provider name               | Type                 | Purpose                                                        |
| ---------------------------- | -------------------- | ---------------------------------------------------------------- |
| `deepseek-inference`         | `openai`             | Used by `openshell inference set` to actually route model calls |
| `deepseek-inference-policy`  | `deepseek-inference` (custom profile) | Attached to the sandbox; contributes network policy (`_provider_deepseek_inference_policy`) describing endpoints and binaries |

The custom profile makes the *intent* (which endpoints, which binaries, which credential)
self-documenting and reusable across sandboxes, without hand-authoring the same
`network_policies` YAML block every time. It does **not** replace the `inference set`
step: there is no built-in `openai` v2 profile that contributes policy, and the routing
provider itself carries none, so the split is genuinely necessary if you want
provider-derived policy.

## Prerequisites

- Working OpenShell installation and gateway.
- A DeepSeek API key from [platform.deepseek.com/api_keys](https://platform.deepseek.com/api_keys).
- Providers v2 enabled on the gateway:
  ```bash
  openshell settings set --global --key providers_v2_enabled --value true
  ```

> **Note:** `export OPENAI_API_KEY=...` only lives for the current shell session. If your
> SSH session drops and reconnects, the variable is gone and any provider created
> afterward using the bare `--credential OPENAI_API_KEY` form (which reads the value from
> your current shell environment) will pick up an empty/stale value, causing a
> `401 Unauthorized` at `openshell inference set` time. Re-export the key at the start of
> every session before running provider commands.

## Full teardown

```bash
openshell sandbox delete codex-deepseek-v2
openshell provider delete deepseek-inference-policy
openshell provider delete deepseek-inference
openshell provider profile delete deepseek-inference
```

## Full rebuild

```bash
# 1. Export your real DeepSeek API key (do this every session)
export OPENAI_API_KEY=sk-your-deepseek-key

# 2. Write the custom provider profile
cat > deepseek-profile.yaml << 'EOF'
id: deepseek-inference
display_name: DeepSeek Inference API
description: OpenAI-compatible DeepSeek chat completions API, routed through OpenShell inference.local
category: inference
inference_capable: true

credentials:
  - name: api_key
    description: DeepSeek API key
    env_vars: [OPENAI_API_KEY]
    required: true
    auth_style: bearer
    header_name: authorization

endpoints:
  - host: api.deepseek.com
    port: 443
    protocol: rest
    access: read-write
    enforcement: enforce
  - host: inference.local
    port: 443
    protocol: rest
    access: read-write
    enforcement: enforce

binaries:
  - /usr/bin/codex
  - /usr/local/bin/codex
  - /usr/local/bin/hermes
EOF

# 3. Lint and import the profile
openshell provider profile lint -f deepseek-profile.yaml
openshell provider profile import -f deepseek-profile.yaml

# 4. Create the inference-routing provider (must be type "openai")
openshell provider create \
  --name deepseek-inference \
  --type openai \
  --credential OPENAI_API_KEY \
  --config OPENAI_BASE_URL=https://api.deepseek.com/v1

# 5. Point inference.local at DeepSeek
#    NOTE: on the Responses API wire format, DeepSeek currently serves only
#    deepseek-v4-flash (v4-pro planned for early August 2026). If "deepseek-chat"
#    no longer resolves for your account, use deepseek-v4-flash here.
openshell inference set \
  --provider deepseek-inference \
  --model deepseek-chat \
  --timeout 120

# 6. Create the policy-carrying provider instance (type: the custom profile)
openshell provider create \
  --name deepseek-inference-policy \
  --type deepseek-inference \
  --credential OPENAI_API_KEY

# 7. Create the sandbox, attaching the policy provider
#    (using "-- true" avoids dropping into Codex's interactive wizard)
openshell sandbox create \
  --name codex-deepseek-v2 \
  --provider deepseek-inference-policy \
  --no-auto-providers \
  -- true

# 8. Write Codex's own provider config into the sandbox
openshell sandbox exec -n codex-deepseek-v2 --no-tty -- bash -c 'mkdir -p ~/.codex && cat > ~/.codex/config.toml << "EOF2"
model_provider = "openshell-deepseek"
model = "deepseek-chat"

[model_providers.openshell-deepseek]
name = "OpenShell DeepSeek Router"
base_url = "https://inference.local/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
EOF2
cat ~/.codex/config.toml'
```

## Verification

```bash
# Confirm inference routing is live
openshell inference get

# Confirm the provider-derived policy block is present and matches the real binary path
openshell policy get codex-deepseek-v2 --full | grep -A 20 "_provider"

# Run a real prompt, non-interactively, from outside the sandbox
openshell sandbox exec -n codex-deepseek-v2 --no-tty -- codex exec --skip-git-repo-check "say hello"
```

A working response should show:

```
provider: openshell-deepseek
model: deepseek-chat
```

followed by an actual model reply and token usage — with no `401 Unauthorized` or
`policy_denied` errors.

## Troubleshooting quick reference

| Symptom                                                              | Likely cause                                                                 | Fix                                                                                          |
| ---------------------------------------------------------------------- | ------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------ |
| `401 Unauthorized` at `openshell inference set`                        | Bad/empty `$OPENAI_API_KEY` at provider-creation time (e.g. after SSH reconnect) | `export OPENAI_API_KEY=<real key>`, then `openshell provider update deepseek-inference --credential OPENAI_API_KEY`, then re-run `inference set` |
| `401 Unauthorized` at `wss://api.openai.com` inside Codex               | Codex is not reading `~/.codex/config.toml` (missing/wrong `$HOME`, or sandbox was recreated after the file was written) | Re-check `echo $HOME` inside the sandbox and rewrite the config file in that exact path         |
| `provider not found` / policy block missing binary                     | Real binary path (`/usr/bin/codex`) not listed in profile `binaries`          | Add the correct path (confirm via `which codex`) and reapply/reimport the profile               |
| `unknown field 'binaries'` when applying policy YAML                   | `binaries` nested inside an `endpoints` entry instead of as a sibling key      | Move `binaries:` to the same indentation level as `endpoints:`, under the named policy block     |
| `failed to parse... resource_version`                                  | Tried to `provider profile update` without exporting the current profile first (exported custom profiles include `resource_version`, which update requires) | `openshell provider profile export <id> -o yaml > current.yaml`, edit that file, then `update`   |
| Garbled terminal output (escape sequences) from `sandbox exec`         | Codex tried to launch its interactive TUI without a proper TTY                | Add `--no-tty` and use `codex exec` (non-interactive subcommand) instead of a bare prompt        |
| `Not inside a trusted directory`                                       | Codex's own git-repo safety check                                             | Add `--skip-git-repo-check` to the `codex exec` call                                             |

## Variant: Claude Code on DeepSeek

DeepSeek also exposes an **Anthropic API format** at
`base_url = https://api.deepseek.com/anthropic`, with automatic model-name mapping:
`claude-opus*` → `deepseek-v4-pro`, `claude-sonnet*` / `claude-haiku*` →
`deepseek-v4-flash`. This makes Claude Code an attractive alternative agent for the same
goal — and today it is the **only path to `deepseek-v4-pro`**, since the Responses API
(used by Codex) is flash-only until ~August 2026.

Key differences from the Codex setup:

- **No `inference set` step.** OpenShell's `inference.local` routes `/v1/messages` only
  for Anthropic-type backends, and whether the `anthropic` provider type accepts a
  base-URL override is not documented. Instead, point Claude Code **directly** at
  DeepSeek's external endpoint and let the sandbox proxy inject the key:

  ```bash
  # Provider holding the DeepSeek key under Claude Code's expected env var
  openshell provider create \
    --name deepseek-anthropic \
    --type generic \
    --credential ANTHROPIC_API_KEY=sk-your-deepseek-key
  ```

- **Reuse the custom-profile pattern for policy**, swapping binaries and adding the
  credential mapping (or hand-author an equivalent `network_policies` block allowing
  `api.deepseek.com`):

  ```yaml
  id: deepseek-anthropic
  display_name: DeepSeek Anthropic-compatible API
  category: inference
  inference_capable: false
  credentials:
    - name: api_key
      env_vars: [ANTHROPIC_API_KEY]
      required: true
      auth_style: header
      header_name: x-api-key
  endpoints:
    - host: api.deepseek.com
      port: 443
      protocol: rest
      access: read-write
      enforcement: enforce
  binaries:
    - /usr/local/bin/claude
    - /usr/bin/claude
  ```

  Confirm the actual Claude binary path with `which claude` inside the sandbox, same as
  the Codex case. Note that although Claude Code has full *default* policy coverage,
  that coverage targets `api.anthropic.com` — reaching `api.deepseek.com` still needs
  this explicit endpoint.

- **Launch with the base URL override** (Claude Code appends `/v1/messages` itself):

  ```bash
  openshell sandbox exec -n claude-deepseek --no-tty -- \
    env ANTHROPIC_BASE_URL=https://api.deepseek.com/anthropic \
    claude --bare -p "say hello"
  ```

  With the model mapping, Claude Code's default model names work as-is; requesting an
  Opus model gets you `deepseek-v4-pro`.

**Compatibility caveats** from DeepSeek's Anthropic-format matrix: image and document
inputs are not supported, `cache_control` is ignored (DeepSeek manages context caching
automatically on its side), and `anthropic-version` / `anthropic-beta` headers are
ignored. Unrecognized model names silently fall back to `deepseek-v4-flash`.

**Privacy trade-off vs. the Codex setup:** this variant sends traffic to an external
endpoint through `network_policies` rather than through the `inference.local` privacy
router. The proxy's placeholder mechanism still keeps the real key out of the agent's
environment (the agent sees an opaque placeholder that the proxy resolves on egress),
but you lose the router's model pinning and header allowlisting.
