# OpenShell Inference Routes, Credential Flows, and the Gateway vs. Proxy Distinction

This document explains how traffic and credentials move through NVIDIA OpenShell for the
two ways an agent can reach a model API — the `inference.local` privacy router and
direct external endpoints governed by `network_policies` — and then clarifies the
architectural difference between the **gateway** and the **proxy**, two terms that are
easy to conflate but name very different components.

Sources: [docs.nvidia.com/openshell](https://docs.nvidia.com/openshell/latest/home)
(Inference Routing, Providers, Providers v2, Manage Gateways, How OpenShell Works,
Sandbox Compute Drivers).

---

## 1. The two routes at a glance

Every byte leaving a sandbox passes through the sandbox proxy — there is no unproxied
path. The two routes differ in *which rules govern the traffic*, not in whether it is
supervised.

| Dimension | Route A — `inference.local` | Route B — external endpoint (`network_policies`) |
|---|---|---|
| Goes through the sandbox proxy? | Yes | Yes — always |
| Can the agent reach arbitrary URLs? | No (only `inference.local` itself) | No — default-deny; only declared hosts, from declared binaries |
| Where does the real key live? | Gateway provider record at rest, proxy memory at runtime; the *agent* never holds it in any form | Same, plus the agent holds an opaque placeholder the proxy resolves on egress |
| Caller-supplied credentials | Stripped and replaced | Placeholder resolved; unresolvable → fail-closed HTTP 500 |
| Model selection | Rewritten to the gateway-pinned model | Whatever the agent requests (any model on that host, your quota) |
| Header control | Per-provider allowlist; everything else stripped | Agent's headers pass, subject to L7 rules |
| L7 enforcement | Router's supported request patterns per backend type | Policy `rules` / `deny_rules` (methods, paths) |
| Backend flexibility | Known provider types only (`openai`, `anthropic`, `nvidia`, Vertex, Bedrock); custom Providers v2 types cannot mount | Any host declared in policy |
| Key-protection requirement | None extra — the key never enters the sandbox | Endpoint must use `protocol: rest` or `tls: terminate` so the proxy sees plaintext |
| Scope | Gateway-wide: one backend for all sandboxes | Per-sandbox, per-policy-layer |
| Residual risk | Agent consumes quota on the pinned model | Agent consumes quota on any model at that host; the key itself is still not exfiltratable |

One-line takeaway: Route A adds credential invisibility, model pinning, and header
stripping *on top of* the same default-deny foundation that Route B already stands on.

---

## 2. Route B in detail: credential placeholder resolution

Lifecycle of one request on the external-endpoint route:

```
Gateway provider record          real key lives only here
        │
        ▼
Sandbox process environment      opaque placeholder only
        │
        ▼
Agent sends HTTPS request        placeholder in auth header / query / path
        │
        ▼
Proxy terminates TLS      ──►    [tls: skip / passthrough → placeholder
        │                         forwarded unresolved → upstream 401]
        ▼
Policy check               ──►    [no host/binary/L7 match → denied]
  host + binary + L7
        │
        ▼
Resolve + inject real key  ──►    [unresolvable placeholder → HTTP 500,
        │                          fail-closed]
        ▼
Forwarded upstream
```

**Setup.** `openshell provider create --credential API_KEY=...` stores the real value in
the provider record on the gateway. At sandbox process launch, the proxy substitutes an
opaque placeholder token into the agent's environment under that variable name. The
agent behaves completely normally — it reads the env var and places the value in a
header, a query parameter, a URL path segment, or inside a base64-encoded Basic-auth
header (the proxy decodes, resolves, and re-encodes that case). It has no idea it is
holding a fake.

**Why TLS termination is the hinge.** The proxy can only swap placeholder → real key if
it can *read* the request, and an HTTPS request is ciphertext unless the proxy
terminates the TLS session itself and re-establishes its own connection upstream. That
is what `protocol: rest` (auto-terminating) or an explicit `tls: terminate` provides.
An endpoint configured as opaque passthrough streams encrypted bytes through untouched,
so the placeholder sails to the upstream server as-is and gets rejected as an invalid
key. Note the failure mode: it is not a key leak (only a useless placeholder escaped) —
it is a broken integration that surfaces as a mysterious `401`. The same constraint is
why dynamic token grants cannot inject into `tls: skip` endpoints.

**The two protective failure branches.**
- *Policy mismatch* is ordinary default-deny: wrong host, wrong binary, or a
  method/path blocked by L7 rules means the request never gets far enough to touch
  credentials at all.
- *Fail-closed resolution*: if the proxy sees a placeholder pattern but cannot resolve
  it (detached provider, expired credential — expired retained credential generations
  are explicitly rejected during resolution), it returns HTTP 500 rather than
  forwarding. Forwarding the raw placeholder would leak internal placeholder tokens
  into the upstream provider's server logs and error responses — exactly the kind of
  quiet exfiltration surface this design closes.

**Scope of the guarantee.** Resolution happens for any request from an allowed binary to
an allowed endpoint; the proxy does not judge intent. The agent can therefore spend your
quota freely within the policy envelope, but it can never extract the key value: it is
not in the agent's memory, not in its environment, and echoing the env var just echoes
the placeholder. The only place plaintext key and agent request coexist is inside the proxy,
for the instant between resolution and upstream forwarding. Limits: the proxy resolves
placeholders in headers, query params, and paths — never in request bodies or cookies
(unless the endpoint enables `request_body_credential_rewrite`).

---

## 3. Route A in detail: the `inference.local` privacy router

Lifecycle of one request on the privacy-router route (e.g. Codex → DeepSeek):

```
Agent calls https://inference.local     dummy key, any model name
        │
        ▼
Proxy intercepts HTTPS                  gateway-scoped route, HTTPS only
        │
        ▼
Strip caller credentials                dummy key discarded
        │
        ▼
Pattern check              ──►          [path not in provider type's
  vs configured provider type            supported patterns → denied]
        │
        ▼
Rewrite model + filter headers          gateway-pinned model; per-provider
        │                               header allowlist, rest stripped
        ▼
Inject real key + forward               to the provider's configured base URL
        │
        ▼
Response streams back                   idle gaps up to 120 s tolerated
```

**Interception (steps 1–2).** The agent's config points its base URL at
`https://inference.local`. Some SDKs require a non-empty API key even though the
sandbox-provided value is never used — any filler such as `unused` works. The route is
gateway-scoped: every sandbox on the active gateway sees the same backend, which is why
it is configured with `openshell inference set` at the gateway rather than in sandbox
policy. Only HTTPS traffic to `inference.local` is intercepted.

**Credential strip (step 3).** The router strips the caller-supplied `Authorization`
before forwarding. This is the structural difference from Route B: there is no
placeholder to resolve because the agent never carried anything credential-shaped
that matters. (The proxy still holds the real key — see §4's "Where the key actually
lives at runtime" — it just does not have to reconcile it with anything the agent sent.)

**Pattern check (step 4).** The request path is matched against what the *configured
provider type* supports. An `openai`-typed backend accepts `/v1/chat/completions`,
`/v1/completions`, `/v1/responses`, `/v1/embeddings`, and model-discovery GETs; an
`anthropic`-typed backend accepts `/v1/messages`. Requests that do not match are
denied. This is why Claude Code (which emits `/v1/messages`) is denied by an
openai-typed backend that happily serves Codex — the wall is wire-protocol, not agent
identity.

**Model pin and header filter (step 5).** The configured model is applied to generation
requests — whatever model name the agent put in the body is overwritten with the
`inference set --model` value, so an agent cannot quietly upgrade itself to a pricier
model. Headers get the same treatment: only a small per-provider allowlist is forwarded
(OpenAI routes: `openai-organization`, `x-model-id`; Anthropic routes:
`anthropic-version`, `anthropic-beta`); all other caller headers are stripped, closing
header-based exfiltration or fingerprinting.

**Injection and forwarding (step 6).** The router pulls the real key from the provider
record and forwards to the configured base URL. Hot reload applies: provider credential
changes and inference updates propagate to running sandboxes within about 5 seconds,
without recreating sandboxes — rotating a key takes effect mid-session.

**Response (step 7).** Streamed back through the router, which tolerates idle gaps of up
to 120 seconds between chunks so long reasoning responses are not cut off — separate
from the overall per-request `--timeout`.

Summary of the contrast: Route B is **resolve-and-forward** (the sandbox participates in
the credential flow via a placeholder); Route A is **strip-and-replace** (the sandbox is
entirely excluded from it).

---

## 4. Gateway vs. proxy: two different components, two different jobs

The terms get conflated because both "sit between the agent and the internet" in a loose
sense. Architecturally they are distinct components, in different places, with different
responsibilities. OpenShell is built around three stable runtime components — the CLI,
the **gateway**, and the **supervisor** — and the proxy is part of the supervisor, not
part of the gateway.

### The gateway: control plane

The gateway is a long-running service (one per environment) that owns **durable state
and decisions**:

- Provisioning and lifecycle of sandboxes through the configured compute driver
  (Docker, Podman, MicroVM, Kubernetes).
- Storing **provider records** (the real credentials) and delivering them to sandboxes
  at startup — and refreshing them via the gateway refresh worker (OAuth2, service
  account JWT, STS strategies).
- Storing and delivering **policy revisions**, runtime settings, and provider profiles;
  composing effective policy (base + `_provider_*` layers) just in time.
- **Inference configuration**: the single provider/model/timeout triple behind
  `inference.local`, and serving inference bundles so sandboxes route requests to the
  correct backend.
- Session records, authorization decisions, and relay coordination — including the SSH
  tunnel endpoint so you can connect to sandboxes without exposing them directly.

Crucially, per the docs: **policy enforcement itself does not happen at the gateway.**
The gateway decides *what the rules are*; it does not sit on the packet path of every
agent request.

### The supervisor and its proxy: data plane / local security boundary

The **supervisor** (`openshell-sandbox`) runs *inside every sandbox workload* and is the
local security boundary. It launches the agent as a restricted child process and
enforces policy at the point where process identity, filesystem access, network egress,
and runtime credentials are actually visible. Its enforcement stack includes the
**proxy** plus OPA (policy evaluation), Landlock (filesystem), and seccomp (syscalls).

The **proxy** is the supervisor's network arm. Everything described in sections 2 and 3
happens here, locally, inside the sandbox boundary:

- Default-deny egress: only policy-declared hosts, only from policy-listed binaries.
- TLS termination for `protocol: rest` / `tls: terminate` endpoints, enabling L7 rule
  enforcement (methods, paths, GraphQL fields).
- Credential placeholder substitution at process launch and resolution on egress
  (Route B), with fail-closed behavior.
- `inference.local` interception and the privacy-router transformations (Route A):
  credential strip, pattern check, model rewrite, header allowlist, key injection.
- Security logging (OCSF structured events for process, network, and HTTP activity).

### How they connect

The relationship is **supervisor-initiated**: each supervisor connects *outbound* to a
known gateway endpoint, authenticates as a sandbox workload, and keeps a live session
open for control traffic and relays. The gateway never needs to dial into sandboxes —
which is what lets the same model work across Docker, Kubernetes, and VM drivers
without solving gateway-to-sandbox reachability per driver. At startup the supervisor
fetches policy, settings, and credentials from the gateway; while running, it **polls
for configuration revisions** — this polling is the mechanism behind the ~5-second hot
reload of credentials, inference config, and provider attach/detach effects.

### Where the key actually lives at runtime

"The proxy injects the real key" invites two wrong readings — that the credential is
handed to the sandbox in the ordinary sense, or that the proxy calls the gateway on
every request to fetch it. Neither is what happens.

**Delivery is push-at-startup plus poll-to-refresh.** The supervisor fetches credentials
from the gateway when it starts and thereafter polls for configuration revisions; the
proxy holds the plaintext value in its own process memory from that point on. There is
no synchronous gateway round-trip on the egress path — putting the control plane on the
data path is exactly what the supervisor/proxy split exists to avoid, which is why agent
traffic does not transit the gateway machine on either route.

**The trust boundary is process-level, not sandbox-level.** This is the distinction the
loose phrase "the sandbox never sees the key" blurs. The supervisor runs *inside* the
sandbox workload, so the credential does cross into the sandbox — it just lands in the
parent process, and the restricted agent child the supervisor spawns never receives it:

| Component | Holds the plaintext key? | How it gets there |
|---|---|---|
| Gateway | Yes — at rest, in the provider record | Written by `provider create` / refresh worker |
| Supervisor's proxy (inside the sandbox) | **Yes — in memory** | Fetched from the gateway at startup, refreshed by polling |
| Agent process (`claude`, `codex`, …) | No | Route B: an opaque placeholder substituted at launch. Route A: nothing real — any filler works |

Two consequences worth holding onto:

- **Rotation is fast but not instant.** Because refresh rides the supervisor's poll loop
  rather than a per-request lookup, a rotated key takes effect mid-session without
  recreating the sandbox — with the ~5-second propagation delay that polling implies,
  not immediately.
- **The residual exposure is the supervisor, not the agent.** Compromising the agent
  process yields a placeholder and nothing else. Reading the key out of memory requires
  compromising the supervisor itself, which is the component doing the confining
  (Landlock, seccomp, privilege drop) rather than the one being confined.

### The division of labor, in one table

| Question | Answered by | Where |
|---|---|---|
| What are the rules? (policy, providers, inference backend) | **Gateway** | Central control plane |
| Is *this specific request* allowed right now? | **Proxy** (in the supervisor) | Inside the sandbox |
| Where is the real API key stored *at rest*? | **Gateway** (provider record) | Central |
| Where is it held *at runtime*? | **Proxy** (in the supervisor) | Inside the sandbox, in process memory |
| How does the proxy obtain it? | **Gateway**, at supervisor startup, refreshed by polling — never per-request | Outbound, control channel only |
| Where is the key inserted into a live request? | **Proxy** | Inside the sandbox, on egress |
| Who restricts the agent process, filesystem, syscalls? | **Supervisor** (Landlock, seccomp, privilege drop) | Inside the sandbox |
| Who creates/deletes sandboxes and serves the CLI/SSH? | **Gateway** | Central |

### Why the split matters for security reasoning

1. **Enforcement is local and always-on.** Because the proxy lives inside each sandbox
   boundary, "does this traffic go through the gateway?" is the wrong question — agent
   traffic does not transit the gateway machine at all on either route. The right
   question is "does it go through the proxy?", and the answer is always yes.
2. **Compromising the agent doesn't reach the rules or the keys.** The rules live at the
   gateway and the real credential lives in the proxy's memory — on the other side of a
   process boundary from the agent, which only ever meets the proxy's enforcement
   surface. Even a fully adversarial agent process is confined to declared endpoints,
   declared binaries, and (on Route B) a placeholder it cannot cash anywhere else. Note
   what this does *not* claim: the key is present inside the sandbox workload, so the
   guarantee rests on supervisor-vs-agent process isolation, not on the key being absent
   from the machine.
3. **Consistency across runtimes.** Because sandbox semantics (proxying, inference
   interception, credential injection, logging) belong to the supervisor, they stay
   identical across Docker, Podman, Kubernetes, and MicroVM drivers — the gateway only
   changes *how* workloads are created, never *what* is enforced inside them.

---

## 5. Mental model recap

- **Gateway** = the brain: state, credentials at rest, policies, inference routing
  config, lifecycle. Consulted, not traversed.
- **Supervisor** = the warden inside every sandbox: restricted agent process,
  filesystem and syscall confinement, and the proxy.
- **Proxy** = the checkpoint on every outbound byte: default-deny, TLS termination,
  L7 rules, placeholder resolution (Route B), privacy-router transformations (Route A).
  It holds the real credential in memory, fetched from the gateway at startup — the
  boundary the agent cannot cross is a process boundary, not the sandbox wall.
- **Route A (`inference.local`)** = strip-and-replace: the agent never touches
  credentials; model and headers pinned centrally.
- **Route B (external endpoint)** = resolve-and-forward: the agent carries a harmless
  placeholder; it keeps model/header freedom within L7 policy.
