# TODO

Organized into three top-level buckets — **Open** (active/未-started work), **Closed**
(landed, kept for history and cross-references), **Parked** (discussed, deliberately
deferred) — with the original per-feature sections nested underneath as area
categories. Sections that were partially done are split: the still-open items live
under Open, the landed items under Closed, both keeping the original section title.

## Open

### `/fact` command (bitacora / bio-log) — remaining

- [ ] Attachment text extraction (md/txt/html parsed directly, PDF via a
      new crate) feeding into the fact prompt -- deferred per the closed
      items below.
- [ ] Quarterly report generation (`/report` or similar): read a quarter's
      `bitacora/<YYYY>/<Jan|Feb|Mar>/...` entries and roll them up into
      Accomplishments / Priorities-alignment sections. Explicitly a "future
      idea" per the user -- alignment-to-priorities in particular needs a
      priorities list to compare against that doesn't exist yet.

### Ingest dedup (idempotent re-ingest of the same link/text)

Re-ingesting the same link or message should **not** create a second note.
Today it does: the only dedup that exists is transport-level on
`update_id`/`event_id` (`seen_updates`/`seen_matrix_events`), which stops a
*single message* being replayed on restart — it says nothing about *content*.
The same URL sent in two separate messages (different ids, minutes or days
apart) passes that dedup and files two near-identical notes. Note that
`notes::unique_path` / `bitacora::unique_base` do **not** help here and in
fact make it worse: they're *anti-clobber* (append `-2`, `-3`) so both copies
are deliberately preserved. This item adds a distinct content-level dedup
layer; don't conflate the two.

- [ ] **Scope: `process_ingest` only** (`kamaji-core/src/worker.rs`), i.e.
      both the no-command default path and the `/ingest <url>` link branch.
      Explicitly **out of scope**: `/ingest <text>` agent-passthrough (writes
      no note) and `/fact` (writes every time by design — see its section).
- [ ] **Dedup key is computed in Rust, before Claude runs.** The message's
      URLs are already extracted up front (`urls::extract_urls`, carried on
      `JobKind::Ingest { urls }`), so the check can happen *before* the URL
      fetch and the paid Claude call — a re-ingest costs zero tokens and no
      network. Do **not** key on Claude's returned `source_url`: it's only
      known post-call (too late to save cost) and Claude may normalize it
      inconsistently run-to-run.
- [ ] **Key derivation (decide, then document the choice):**
      - URL messages → a normalized URL. At minimum: lowercase host, strip a
        trailing `/`, drop the fragment. Open question: strip common tracking
        params (`utm_*`, `fbclid`, …)? Recommend yes, a small fixed denylist,
        since those are exactly what makes "the same link" look different.
      - Text-only messages (no URL) → a hash (e.g. SHA-256) of the
        whitespace-normalized `raw_text`. Exact-ish repeats dedupe; genuinely
        different text does not. Do **not** try fuzzy/semantic matching here —
        that belongs to Claude, not a dedup key.
- [ ] **Multi-URL messages:** one message can carry several URLs but produces
      one note. Decide the rule and state it: recommend "duplicate if **every**
      URL in the message is already known" (a message that adds even one new
      link is still worth filing), and record *all* of a message's URL keys
      pointing at the written note so any of them dedupes a later single-link
      resend.
- [ ] **Storage stays 100% redb** (CLAUDE.md non-negotiable — kamaji's own
      persistence never touches sqlite). Add a new table in
      `kamaji-core/src/db.rs`, e.g. `ingested_keys<&str, &str>`: dedup_key →
      relative note path. Persisted so dedup survives restarts, not just an
      in-process set.
- [ ] **Record the key only *after* a successful note write + commit.** If a
      skipped/failed ingest wrote the key, a transient failure would
      permanently suppress a later real attempt at the same link — a silent
      data-loss bug. Key goes in only once the note exists on disk and is
      committed.
- [ ] **Don't fail silently** (ingest-path contract): on a duplicate, reply on
      the originating platform that it was already ingested, ideally citing the
      existing note's path from the stored value — not a silent drop. Skip the
      fetch, the Claude call, the note, the commit, and the push.
- [ ] **Tests:** two ingests of the same URL → one note + an "already
      ingested" reply, no second commit; dedup survives a simulated restart
      (persistence, not just a live set); two *different* URLs → two notes;
      exact text-only repeat deduped, a materially different text not;
      multi-URL "all-known vs one-new" boundary. Keep the existing
      `unique_path` suffix tests green — this layer sits in front of them, it
      doesn't replace them.

### Goal/TODO alignment (future phase, tag-based first)

Explicitly deferred — the user's own framing is "later we could run a
loop." Not designed in detail yet; captured here so `/todo` and `/goal`
are built with this eventual consumer in mind rather than accidentally
incompatible.

- [x] **Phase 1 — tag-based, read-only, decided as `CommandMode::Sync`:**
      a `cmd_align` in `commands.rs` alongside `cmd_status`/`cmd_history`
      — no worker module, no Claude call, no schema change, since it only
      loads open `TodoEntry` + open `GoalEntry` (both already carry
      `tags: Vec<String>`) and groups by shared tag, entirely in Rust.
      Report has three sections: goals with no tag-overlapping open TODO,
      TODOs with no tag-overlapping open goal, and matched pairs grouped
      by shared tag. No args for v1 (filters can come later). Output is a
      report, not an automated action — matches the "reporting first, not
      auto-response" principle already used for the self-protecting-kamaji
      idea below. Corrects the "every loop candidate gets a worker job +
      schema" framing in the design note below — this one needs neither.
      **Superseded** by the "`/align` auto-generates links" bullet further
      down: `cmd_align`/`CommandMode::Sync`/no-worker-module are no longer
      current — `/align` now writes and is `CommandMode::Queued` with its
      own `worker/align_job.rs`. Left here as an accurate record of what
      Phase 1 actually shipped as.
- [x] ~~**Reachable from the `kamaji` CLI/socket too, not just
      Telegram/Matrix:** needs a `CliRequest::Align` variant (`ipc.rs`)
      following the existing per-command-variant pattern — the same gap
      `/goal` already has (no `CliRequest::Goal` today). This is what
      lets a systemd timer trigger it (see "Scheduled analysis loops"
      below).~~ — added `CliRequest::Align` (`kamaji-core/src/ipc.rs`,
      maps to `("align", [])`, no new args), wired into the `kamaji` CLI as
      `Command::Align` (`kamaji/src/main.rs`) and documented in README's
      CLI Usage section. No socket/REST transport changes needed: both
      already dispatch any `CliRequest` generically via
      `transport::run_cli_style_request` → `dispatch_routed_job` →
      `commands::mode`/`worker::dispatch_sync_command`, which already knew
      about `align` as a `CommandMode::Sync` command since Phase 1 landed.
- [x] ~~**Scheduled trigger, when it exists: Unix socket, not REST.** A
      oneshot `kamaji-align.service` running `kamaji align` locally, same
      pattern the auto-update-cron design uses for its own timer — REST's
      TOTP/session login is built for an interactive human, not an
      unattended timer.~~ — added `deploy/kamaji-align.service` (oneshot,
      `ExecStart=kamaji align` over the local socket, `Requisite=kamaji.service`
      so it fails fast rather than silently no-op-ing if the daemon is down)
      + `deploy/kamaji-align.timer` (`OnCalendar=hourly`, `Persistent=true`
      so a missed run while the VM was off still fires — cadence is a
      starting point, tunable per-deployment). Documented in README's new
      Deployment step 8.
- [x] ~~**Reply delivery for a timer-triggered run: journal only for v1, no
      new push mechanism.** A systemd-triggered run has no live chat to
      reply into, so its output is just the CLI process's stdout (piped
      to the journal by the unit). Proactive Telegram/Matrix delivery
      waits for the already-planned `CliRequest::Notify` relay (see
      self-protecting-kamaji below) and reuses it, rather than building a
      second "relay to configured chat" mechanism now.~~ — `kamaji-align.service`
      has no `StandardOutput=`/`StandardError=` override, so systemd's
      default journal capture is exactly this: `journalctl -u
      kamaji-align.service`. No Notify-relay wiring added — still waits on
      that item.
- [x] ~~**Phase 2 — explicit linkage:** once tag-matching proves noisy (a
      shared tag like `#work` says nothing about actual alignment), add
      an explicit reference field — e.g. `linked_goal: Option<EntryKey>` on
      `TodoEntry` (see "id-less reference format" under Closed — entries
      are addressed by `EntryKey`/`YYYY-MM-DD-N` now, not a `u64` id),
      written as an extra bracketed segment on the rendered line (`LINE_RE`
      gains an optional capture group) or via a new `/todo link <todo_key>
      <goal_key>` subcommand. Needs a decision on whether tags are dropped
      once linkage exists or kept as a secondary/fallback signal.~~ —
      landed via a bigger shape change than the line-segment idea above:
      todos/goals moved from lines in shared month-files to **one markdown
      file per entry** (`todo/<YYYY>/<EntryKey>.md`, `goals/<YYYY>/<EntryKey>.md`),
      each with real OKF frontmatter (`checklist/entry_file.rs`, new) —
      mirroring how `notes/`/`bitacora/` already store one file per item,
      rather than bolting a bracketed segment onto the old line format.
      `/todo link <ref> <goal key>` (`todo::link_to_goal`) writes a
      **one-directional** Obsidian wikilink (`link: "[[goals/<YYYY>/<key>]]"`)
      into the todo's frontmatter; nothing is written to the goal file —
      Obsidian's own backlink graph computes the reverse view, confirmed by
      manual smoke test. Tags are kept as the fallback signal, not dropped:
      `/align` (`align_report`) now excludes an explicitly-linked TODO from
      both tag-overlap sections and lists it once under a new "Explicitly
      linked TODOs" section instead — `/align` itself still only reports,
      never writes a link (kept 100% `CommandMode::Sync`, no auto-linking).
      **Superseded**: this last sentence was a misreading of the user's own
      intent, corrected in the "`/align` auto-generates links" bullet below
      — `/align` now writes, `CommandMode::Queued`, the report reshaped
      accordingly ("Explicitly linked TODOs" is gone, replaced by "Goals
      and their linked TODOs").
      **Backward compatible by design, migration deferred separately**:
      `list_entries`/`resolve`/`reopen` dual-read old month-files and new
      per-entry files (a file's stem parses as `EntryKey` → new format,
      else legacy), so every already-committed todo/goal keeps working
      unmigrated; `next_line_for_day` continues the per-day line counter
      across both formats so a new-format entry can never collide with an
      un-migrated legacy one. v1 limitation: only an already-new-format
      goal can be a link target (`goal::entry_exists`) — a goal still in a
      legacy month-file can't be linked to until migrated. No `/todo
      unlink`, and closing an entry still adds no timestamp (kept the
      existing "git history is the audit trail" philosophy rather than
      adding `closed_at`). Migrating already-committed legacy files into
      the new per-entry format is explicit, separate future work — not
      done here.
- [x] ~~**`/align` should generate the links itself, not just report tag
      overlap** — the actual point of the whole feature (user's own
      framing: "help me achieve my goals by fulfilling the supporting
      todos"), corrected after an earlier misreading that treated `/todo
      link` as the only write path and kept `/align` read-only.~~ — `/align`
      moved to `CommandMode::Queued` (was `Sync`) with a new
      `worker/align_job.rs` (`cmd_align`/`align_report` moved out of
      `commands.rs` entirely), since it now writes TODO→goal links and
      commits — same "no concurrent writes to the notes repo" guardrail as
      `/todo`/`/goal`. Per run: for every open TODO, goals it tag-overlaps
      with (excluding ones already linked) become link candidates; within
      threshold, all candidates get auto-linked in one batched commit
      (`"Align: linked N todo(s) to matching goal(s)"`); `/align` itself
      never decides *not* to write for any other reason (a per-pair write
      failure is logged and skipped, not treated as a whole-job failure).
      `Entry.link: Option<String>` → **`Entry.links: Vec<String>`**: a TODO
      can support several goals (confirmed both Obsidian — a frontmatter
      field can hold a YAML list of wikilinks, backlinks work identically —
      and OKF — no cardinality constraint on a custom field — support this
      natively), so `checklist::add_link`/`entry_file::add_link` are
      idempotent-append, not replace; `todo::link_to_goal` returns
      `LinkOutcome::{Linked, AlreadyLinked}` (mirrors `ResolveOutcome`/
      `ReopenOutcome`). New `ALIGN_NOISY_TAG_THRESHOLD` (default 3, `Config`
      in `config.rs`): a TODO whose tag overlap would connect it to more
      candidate goals than this is treated as a too-generic/noisy match —
      auto-linked to **none** of them, surfaced in a new report section
      ("TODOs with too many candidate goals, needs manual `/todo link`")
      instead of guessing. Report reshaped: "matched by shared tag" as a
      separate category is gone (tag overlap is now the *mechanism* that
      creates a link, not a persistent report bucket) — replaced by "Goals
      and their linked TODOs" (grouped by goal, a link created this run
      marked `(new)`), alongside the existing gap-finder sections ("Goals
      with no linked TODO" / "TODOs with no linked goal"). No `/todo
      unlink` — the agreed mitigation for a bad auto-link is the noisy-tag
      threshold, not a removal command; hand-editing the `link` list (or
      removing the shared tag) is the v1 workaround if one turns out wrong.
- [ ] **Migrate legacy month-files to the new per-entry format** (the
      follow-up explicitly deferred from Phase 2 above): rewrite each
      already-committed `todo/<YYYY>/<MM>.md`/`goals/<YYYY>/<MM>.md` line
      into its own `<EntryKey>.md` file so every pre-existing entry becomes
      link-eligible (`goal::entry_exists`/`todo`'s equivalent currently
      only recognizes new-format files as valid link targets) and the
      dual-read fallback path in `checklist::list_entries`/`set_status` can
      eventually be retired. Needs its own pass: a one-time script, not an
      in-process migration, and a decision on whether the old month-files
      are deleted after or left in place as a backup.
- [ ] **Phase 3 — "accomplish them":** the part of the idea where the
      loop doesn't just report but drives TODOs toward completion. Out of
      scope until Phase 1/2 exist and are useful on their own; likely
      needs its own design pass (what does "accomplish" mean
      operationally — file a `/fact`? auto-resolve a TODO? draft a
      note?) rather than being bolted onto the alignment report.

### Scheduled analysis loops (report/alignment/linking) — design note

Broader framing for "later we could run a loop" above, prompted by a
design discussion covering four candidate loops: a monthly/quarterly
top-N notes report, the goal/TODO alignment pass, a bitacora-facts-to-goals
linking pass (**landed as `/demonstrate`, see the checked item below** —
renamed from the working name `/linkgoals` used elsewhere in this section
to match its actual framing: find facts that demonstrate a goal), and a
"research this topic to help close that TODO" pass. Not designed in detail
yet for the remaining two — captured so the shape below is what future
work on either should converge on, rather than one-off designs.

- [ ] **A "loop" is just another command, not a new subsystem — but not
      every candidate needs the `Queued` + Claude + schema shape.**
      `/report [monthly|quarterly]`, `/linkgoals [period]`, and
      `/research <todo_id>` invoke Claude and touch the notes repo/tools,
      so each is a `CommandSpec` in `commands.rs` (`CommandMode::Queued`)
      with its own args/defaults, its own `worker/*_job.rs` module, and
      its own strict-JSON schema in `prompt.rs` — same pattern as
      `ingest_job`/`fact_job`/`todo_job`/`goal_job`/`align_job` (`/align`
      writes now too, via its own `worker/align_job.rs` — see Goal/TODO
      alignment above; it just never calls Claude, so no schema needed
      there). Runnable manually (`/report` on Telegram/Matrix) exactly the same
      way it'd run on a schedule; no separate "loop mode" code path.
      Rejected alternative: one generic config-driven "prompt +
      data-query" engine — would fight the explicit-typed-schema
      discipline every other job in this codebase follows, for
      flexibility none of the four candidates actually need.
- [ ] **Trigger is external, not an in-process scheduler.** A systemd
      timer (same pattern as `deploy/kamaji-align.timer`, already landed —
      see Goal/TODO alignment above, and the deferred auto-update-cron
      design below) hits the existing `socket`/`rest` transport with the
      command's default args — every one of these candidates (`/report`/
      `/linkgoals`/`/research`/`/align`) is `Queued` and goes through
      `enqueue` and the single sequential worker exactly like a
      human-issued command. No new scheduling primitive inside `kamajid`.
- [ ] **Consider an importance-threshold trigger alongside pure calendar
      cadence.** Stanford's "Generative Agents" (Park et al. 2023)
      architecture fires its periodic reflection/synthesis step when the
      *cumulative importance* of new memories crosses a threshold, not on
      a fixed clock — and its own ablation found agents degrade
      specifically over long horizons without that step. Kamaji already
      computes an `importance` score (1-5) on every ingest, so "reflect
      once N points have accumulated since the last run" is close to free
      to add and may be a better fit than a fixed monthly timer for
      `/report`/`/linkgoals`. Letta/MemGPT's "sleep-time compute" (a
      background agent updating shared memory during idle periods) is the
      production analog worth reading if this gets designed in earnest.
- [ ] **`/research <todo_id>` is one `agent::invoke` call, not kamaji-level
      multi-agent orchestration.** Clarified during design discussion:
      "fan out to subagents" means Claude/Codex doing that internally
      (e.g. Claude Code's own Task-tool subagents), invisible to kamaji —
      not kamaji spawning multiple concurrent agent processes, which would
      violate "no concurrent Claude runs against the same working
      directory." Anthropic's own multi-agent research system (orchestrator
      process spawning parallel subagent *processes*, ~15x token cost of a
      single-agent call) is the wrong reference point here — not what this
      is.
- [ ] **`/research` needs its own invocation profile, not
      `invoke_claude`'s as-is.** `claude.rs::invoke_claude`
      (`kamaji-core/src/claude.rs:16-38`) passes one fixed
      `["-p", "--output-format", "json"]` argv shared by every call today,
      which works because the existing ingest/fact/todo/goal prompts are
      pure text-to-JSON completions needing no tool use. A research job
      needs Claude to actually use tools (WebSearch/WebFetch/Task/Read),
      which the CLAUDE.md ban on `--dangerously-skip-permissions` rules
      out doing unsafely. Use `--permission-mode plan` instead: it lets
      Claude research and fan out subagents freely while staying
      structurally unable to `Write`/`Edit`/`Bash` the notes repo, and its
      natural output (the plan/findings) becomes the job's reply — matching
      the "reporting first, not auto-response" principle already used for
      the goal/TODO alignment idea above. Needs its own argv variant and
      likely a longer timeout budget than the current single-shot calls,
      since a multi-turn subagent run takes meaningfully longer.
- [x] ~~**The bitacora-facts-to-goals linking pass**~~ — landed as
      `/demonstrate` (`worker/demonstrate_job.rs`, `CommandMode::Queued`,
      no schema-less shortcut like `/align` since it *does* call Claude by
      default): auto-links open goals to bitacora facts that demonstrate
      them, writing a `demonstrated_by: [...]` wikilink list into the
      goal's own frontmatter (`goal.rs::CFG.link_field`, flipped from
      `None`) rather than the fact side, since facts aren't
      `EntryKey`-addressable and can't hold an outbound link the way a
      todo can (`bitacora.rs` was write-only before this — added
      `FactRecord`/`list_facts` as its first read-back capability).
      Two-stage matching, a deviation from `/align`'s pure-tag-overlap
      mechanism per explicit user request: Stage 1 is the same cheap
      per-entry tag-overlap + noisy-threshold candidate generation as
      `/align` (`DEMONSTRATE_NOISY_TAG_THRESHOLD`, default 3, applied per
      fact instead of per todo, since a fact is what generates candidate
      goals in this direction); Stage 2, on by default
      (`DEMONSTRATE_SEMANTIC_MATCH=true`), asks Claude per goal-with-
      candidates which of the tag-matched facts *actually* demonstrate it
      (`prompt::run_demonstrate_prompt`/`DemonstrateResult`) rather than
      just sharing a tag — `false` falls back to `/align`'s original pure
      tag-overlap. Scope defaults to the **current quarter**, not
      unbounded history like `/align` (facts have no open/closed
      lifecycle, so nothing bounds "all facts ever" on its own) —
      `/demonstrate [all|YYYY-Q1..4]` widens or shifts it explicitly
      (`demonstrate.rs::Scope`/`parse_scope`). Deployed the same way as
      `/align`: `deploy/kamaji-demonstrate.service`+`.timer`,
      `CliRequest::Demonstrate`, `kamaji demonstrate [scope]`.
      **v1 gaps, not built**: no manual `/goal link-fact` escape hatch for
      a noisy fact (unlike `/align`, which already had `/todo link` before
      auto-linking existed) — hand-editing `demonstrated_by:` or tuning
      the threshold is the interim workaround; and nothing caps how many
      candidate facts one goal's semantic-match Claude call can receive
      (only `DEMONSTRATE_NOISY_TAG_THRESHOLD` bounds how many *goals* one
      fact can match) — a broad-tag goal combined with `all` scope could
      produce a large prompt, acceptable to defer per the "start simple,
      evolve" trajectory `/align` itself followed.

### Auto-update cron (check for new release, run `update.sh`)

Today `deploy/update.sh` is a manual, over-SSH, sudo invocation: it
unconditionally `git pull`s, `cargo build --release`s, installs both binaries,
and `systemctl restart`s. The idea is a scheduled check on the VM that runs it
**only when there's actually a new release**, so the box keeps itself current
without a human SSHing in.

- [ ] **Define "release" precisely: a new `vX.Y.Z` git tag**, not any push to
      `main`. Releases here are already tagged (`git tag`: `v1.3.2`, …) with a
      matching `release: vX.Y.Z` commit. Keying off tags (not the latest
      `main` commit) means WIP commits don't trigger an unattended rebuild +
      restart of the daemon — only deliberate releases do. Check the remote
      cheaply with `git ls-remote --tags origin 'v*'`, pick the highest
      semver, compare against the deployed version (the current tag, or
      `kamaji --version` / Cargo.toml `version`).
- [ ] **Only build/install/restart when the remote tag is newer.** `update.sh`
      today always pulls and always restarts; splitting the "is there a new
      release?" check out front avoids needless `cargo build`s and, more
      importantly, needless daemon restarts that interrupt the single worker
      mid-job. Either add a `--if-newer` guard to `update.sh` or wrap it in a
      small `check-and-update.sh` the timer calls — recommend the wrapper so
      the existing manual `update.sh` stays a dumb, always-does-it tool.
- [ ] **systemd timer, not crontab** — consistent with the rest of the
      deployment (systemd-managed, `Restart=always`). Add
      `deploy/kamaji-update.service` (oneshot, runs the wrapper as root) +
      `deploy/kamaji-update.timer` (e.g. daily, with `Persistent=true` so a
      missed run while the VM was off still fires). Document the cadence.
- [ ] **Concurrency + safety:** `flock` so two timer firings can't overlap a
      build; rely on `update.sh`'s existing `set -euo pipefail` + install-only-
      after-build ordering so a failed `cargo build` aborts *before* the
      restart (never install/restart a half-built binary). Pin the pull to the
      release tag (`git fetch --tags && git checkout <tag>`) rather than
      pulling a moving branch, so what's built is exactly what was tagged.
- [ ] **Surface failures — don't let an auto-update fail silently.** A failed
      build/restart on an unattended box is invisible otherwise. Natural fit
      with the self-protecting-kamaji `CliRequest::Notify` idea below (relay a
      one-line "update to vX.Y.Z failed" to the configured chat via the
      **`Sync`** path); until that exists, at minimum `systemd`
      `OnFailure=` on the update unit.
- [ ] **Security note (this is unattended root code-execution from a git
      remote):** the timer runs `cargo build` + `systemctl restart` as root on
      every release, so whoever can push a `v*` tag to `origin` can run
      arbitrary code as root on the VM at the next tick. Acceptable only
      because the repo is single-maintainer and the notes/deploy PATs are
      already scoped (see VM hardening §2.1) — but call it out in
      `docs/hardening.md`, and consider requiring the tag be signed
      (`git tag -v`) before the wrapper acts on it. Decide this explicitly
      rather than shipping auto-root-exec by omission.
- [ ] **Docs:** README Deployment section gains the "it auto-updates on new
      tags" note and how to disable it (`systemctl disable --now
      kamaji-update.timer`) for a pinned/manual box.

### VM hardening

Full plan and rationale in `docs/hardening.md`; clean forensic baseline captured
2026-07-22 (same doc, "Baseline" section) with the reusable audit script at
`deploy/security-baseline.sh`. Ordered high-value-first; do Tier 1 in one sitting,
Tier 2/3 as appetite allows.

Tier 1 (high impact, low complexity):
- [ ] SSH: run `sshd -T` to confirm `passwordauthentication no` +
      `kbdinteractiveauthentication no`; add the `sshd_config.d/99-hardening.conf`
      drop-in (incl. `AllowUsers`) if either is still `yes`. Keep a session open,
      verify in a second one. (§1.1)
- [ ] Firewall both layers: firewalld + Hetzner Cloud Firewall, default-deny
      inbound, allow only 22 (+ 80/443 for Caddy). **Unverified at baseline** --
      confirm nothing else is exposed. (§1.2)
- [ ] Move `TELEGRAM_BOT_TOKEN` / `MATRIX_ACCESS_TOKEN` out of `Environment=`
      into a root-only `EnvironmentFile` (mode 600); drop the placeholder token
      line from `deploy/kamaji.service`. (§1.4)
- [ ] Enable `dnf-automatic` for security updates. (§1.5)
- [ ] Sandbox the systemd unit via a `hardening.conf` drop-in (test ingest
      end-to-end after -- **no** `MemoryDenyWriteExecute`/`ProtectHome`, they
      break the `claude` child and credential files). (§1.3)

Tier 2/3 (as appetite allows):
- [ ] Set 90-day expiry on both GitHub PATs; verify notes PAT is repo-scoped,
      Contents:read/write only. (§2.1)
- [ ] Socket perms: move `KAMAJI_SOCKET_PATH` into a `chmod 700` dir. (§2.2)
- [ ] fail2ban on sshd (§2.3); nightly backup of `kamaji.redb` +
      `matrix-store` + secrets, not just the git-pushed notes (§2.4).
- [ ] **SSRF fix (code, from baseline finding):** block loopback / link-local /
      RFC1918 destinations in the URL fetcher -- the ingest worker was observed
      fetching `127.0.0.1:4317`, so the metadata-endpoint vector is live. (§3.2)
- [ ] Tuwunel/Caddy: apply the same systemd sandboxing; confirm open
      registration is disabled; subscribe to Tuwunel releases (not covered by
      `dnf-automatic`). (§3.3)

Self-protecting kamaji (design, user's idea -- reporting first, not auto-response):
- [ ] `CliRequest::Notify { text }` variant + a **`Sync`-path** handler (must NOT
      go through the queue, or an alert about a stuck worker queues behind the
      stuck worker) that relays to the configured chat; never written to the
      notes repo. Fed by systemd `OnFailure=` hooks and a fail2ban action.
- [ ] External dead-man's-switch heartbeat (healthchecks.io-style) so total VM/
      daemon death is detectable -- kamaji can't report its own death. Alert on
      state-changes vs the documented baseline (new login IP, new key, new
      listener), not on raw scan volume.

### Agent backend abstraction (`agent.rs`) — remaining

Context (full background and landed shape in the Closed section below):
OpenShell was wired in as a pluggable *runner* (direct exec vs OpenShell-
wrapped), orthogonal to the *agent* axis (Claude vs Codex). Two items from
that effort are still open:

- [ ] **OpenShell auth beyond v1's anonymous/plaintext.** `OpenShellConfig` has
      no `AuthConfig` surface today — it targets a local, plaintext,
      single-VM gateway (`openshell_sdk::ClientConfig::auth: None`). If
      kamaji's OpenShell gateway ever moves off a trusted local network, wire
      `openshell_sdk::AuthConfig::Oidc` (bearer token + refresh) or
      `AuthConfig::EdgeJwt` (Cloudflare Access tunnel) through a new gated env
      var block, following the same opt-in-when-set convention.
- [ ] **Pinned-dependency maintenance.** `openshell-sdk` isn't on crates.io, so
      the `git`/`rev` pin in `Cargo.toml` has no automated staleness check —
      bumping it is a manual, occasional chore: edit `rev` to a newer tagged
      commit, then re-run `cargo build`, the full test suite, and the
      `#[ignore]`d `openshell_smoke_test_oversized_stdin` against a real
      gateway before merging. Revisit if/when `openshell-sdk` publishes to
      crates.io — a version range would then be strictly better than a pin.

### Per-command agent override (default agent, overridable per invocation) — design note

Prompted by a design discussion: today `Config::from_env`
(`kamaji-core/src/config.rs:202-213`) collapses everything to one
`agent_flavor: AgentFlavor` + one `agent_bin: String` at startup — if
`AGENT_FLAVOR=claude`, `CODEX_BIN` is never even read from the
environment, so the daemon has no idea what the *other* flavor's binary
even is once it's running. The idea is a command-level `--agent
<flavor>` override (e.g. `/research --agent codex <todo_id>`) on top of
a configured default, plus an `/agents` command to list what's usable.
Not designed in detail yet — captured so future work on the agent axis
converges on this shape rather than bolting an override onto the
single-flavor assumption baked in today.

- [ ] **Override surface reuses the existing command-args path**, no new
      dispatch mechanism: an optional `--agent <flavor>` token in
      `JobKind::Command { args }`, parsed by whichever `worker/*_job.rs`
      module invokes an agent, picking the flavor/bin for that one
      `agent::invoke` call instead of `state.config`'s default.
- [ ] **`Config` needs to stop collapsing to one bin.** Replace the single
      `agent_flavor`/`agent_bin` pair with a small map of *enabled*
      flavors → resolved bin path, plus a `default_flavor`. Requires a new
      explicit "enabled" concept — `AGENT_ENABLED=claude,codex` — that
      doesn't exist today; without it, a per-command override could
      silently invoke a binary nobody actually provisioned on this VM,
      the same un-validated-pairing gap CLAUDE.md already flags for
      OpenShell+Codex, one layer earlier. An override naming a flavor
      outside the enabled set is a usage-error reply, not a silent
      fallback to default (same "don't blur unknown vs known" principle
      as unrecognized commands).
- [ ] **Backward compatibility for `AGENT_FLAVOR`.** Its meaning subtly
      shifts from "the only usable flavor" to "the default flavor" — needs
      `AGENT_ENABLED` to default to just the configured default flavor
      when unset, so existing deployments that only ever set
      `AGENT_FLAVOR` keep today's single-flavor behavior byte-for-byte,
      and only start accepting overrides once `AGENT_ENABLED` is
      explicitly widened.
- [ ] **`/agents` list command**, `CommandMode::Sync` (like `/status`/
      `/history` — read-only, no queue, no `job_history` record) printing
      the enabled flavors and which one is default.
- [ ] **`override.conf` grows, doesn't change shape.** Stays flat
      `Environment=` lines (the one config format this codebase uses
      everywhere — Matrix/OpenShell/REST all opt in the same way), just
      more of them, e.g.:
      ```
      Environment="AGENT_FLAVOR=claude"
      Environment="AGENT_ENABLED=claude,codex"
      Environment="CODEX_BIN=/opt/codex/bin/codex"
      ```

### TODO management web UI (`/api/todos` + `/app`)

Prompted by a user request for "an easy to use web app to Add/Update/Delete
TODO items, protected the same way as the REST API, with a token." Decided
shape (superseding the earlier open questions this section started as):

- [x] **Reuses the existing REST API's TOTP-login -> bearer-session auth
      as-is, no second token scheme.** `kamajid`'s REST API
      (`kamajid/src/transport/rest.rs`, `kamaji-core/src/auth.rs`) already
      does exactly "protected with a token": `/auth/login` (TOTP code ->
      bearer token, rate-limited) then `Authorization: Bearer <token>` on
      every call, sessions in the existing `SESSIONS` redb table. The new
      `/api/todos/*` routes and the static `/app` frontend are additions to
      this same router/process -- no new port, no new service to secure.
- [x] **Structured JSON, not a UI wrapper around `CliRequest::Todo`'s text
      replies.** `GET /api/todos` reads `checklist::list_entries` directly
      (a filesystem read, no git write, so it bypasses the queue the same
      way `/status`/`/history` already do) and returns `Vec<Entry>` as JSON
      (`checklist::Entry`/`Status`/`EntryKey` gained `Serialize`). Writes
      (add/resolve/reopen/edit/delete) go through a new `JobKind::TodoApi`
      variant (`kamaji-core/src/queue.rs`) -- still `Queue::enqueue` and the
      single sequential worker, same guardrail as every other write to the
      notes repo, just carrying a typed `TodoApiOp` instead of the
      chat-command `(name, args)` shape, and replying with a JSON string
      (`{ok, message, entry}`) that the REST handler passes straight
      through rather than a human-formatted chat reply.
      `kamaji_core::worker::todo_api_job` implements each op.
- [x] **Edit and hard-delete are new capabilities, deliberately scoped to
      this access point only** -- explicit user call: resolve/reopen reuse
      the existing `checklist::open_entry`/`close_entry`, but in-place
      text/tag editing and permanent deletion have no chat/CLI equivalent
      and aren't being added as one (no new `/todo edit|delete` subcommand,
      `TodoAction`/`parse_command` untouched) -- the whole point was richer
      input (tags/links/emoji) in a browser, not a new chat surface.
      - **Edit** (`checklist::edit_entry`/`entry_file::edit`): only
        supported for the new per-entry file format (same boundary
        `entry_exists`/linking already draw) -- re-renders the file with
        new tags/text while preserving the original `timestamp:`,
        `status:`, and `links:`. A legacy-format entry (or missing key)
        replies "not found, or predates the editable per-entry format"
        rather than silently no-op-ing.
      - **Hard delete** (`checklist::delete_entry`): removes the entry
        file outright and commits the removal (`git rm` via
        `commit_and_push`) -- a real deviation from the "git history is
        the audit trail, nothing is ever removed" precedent set by
        `/todo`/`/goal`'s resolve-not-delete design, accepted here because
        the user explicitly asked for it and the deletion itself is still
        committed (so it's *visible* in history, just not *reversible*
        from a running note). Same new-format-only boundary as edit.
        Frontend shows a clear warning before calling this endpoint.
- [ ] **Frontend**: single static page at `GET /app` (no build step --
      plain HTML/CSS/JS, embedded via `include_str!`), TOTP login screen
      storing the bearer token in `localStorage`, then list/add/edit/
      resolve/reopen/delete against `/api/todos`. Responsive for both a
      laptop and a phone (flexbox/grid + a viewport meta tag, no separate
      mobile build). No CORS needed -- same-origin as the REST API it
      calls.
- [x] ~~**Not done in this pass**: no equivalent web UI for `/goal` (todo
      only, per the original ask) -- the same `checklist` primitives would
      make a `goal.rs` version straightforward to add later if wanted.~~ --
      done, see "Goals and facts in the web UI" below.

### Goals and facts in the web UI (`/api/goals`, `/api/facts`)

Prompted by "extend `/app` beyond todos so goals and facts are also viewable
and editable". Todos and goals came out symmetric; facts deliberately did not.

- [x] **Generalised the checklist API rather than duplicating it.**
      `TodoApiOp` became `ChecklistApiOp` + a `ChecklistDomain` tag, with one
      worker handler (`worker/checklist_api_job.rs`, was `todo_api_job.rs`)
      parameterised by `checklist::Config` and one set of domain-generic REST
      handlers. Chosen over a parallel `GoalApiOp`/`goal_api_job.rs` (a
      smaller diff, but ~250 duplicated lines to keep in sync, and a third
      copy the moment facts arrived) because it mirrors how `checklist/mod.rs`
      already unified the two domains. Message wording comes from `Config`
      (`command_name`/`closed_verb`/`close_subcommand`) plus a capitalised
      display noun, so no domain string is hardcoded twice.
- [x] **Old queue payloads still deserialize.** `JobKind::TodoApi` is kept as
      a read-only legacy variant (never constructed again) alongside the new
      `ChecklistApi { domain, op }`; the worker maps it to
      `ChecklistDomain::Todo`. `op` is nested rather than `#[serde(flatten)]`ed
      precisely because the legacy shape already flattens `ChecklistApiOp`'s
      own `"op"` tag into the same object as a string -- the two couldn't
      coexist flattened. Covered by a test feeding the literal old JSON.
- [x] **Editing a goal must not clobber `demonstrated_by`.**
      `checklist::edit_entry` already preserved status/links/timestamp, but no
      test covered a *linked* entry. Added one in `checklist/mod.rs` and one at
      the real goal `CFG` in `goal.rs`.
- [x] **Deleting a goal is refused while any todo links to it**, naming the
      linking todos. Chosen over dangling wikilinks (silently degrades
      `/align` and the Obsidian graph) and over scrubbing inbound links (one
      click rewriting N files the user never sees). Enforced in
      `worker::checklist_api_job` -- the cross-domain question can only be
      answered by a caller that sees both domains, and `checklist/mod.rs` must
      stay domain-agnostic. The UI greys the button out with the same reason
      before the click; the server is what actually holds the line.
- [x] **Facts got a fuller read-back, not a widened `FactRecord`.**
      `bitacora::FactDetail` (+ `read_fact`/`list_fact_details`) is a second
      projection of the same private parser, so `/demonstrate` doesn't start
      carrying a body and an attachment path it never looks at.
- [x] **Three fact-editing guarantees, enforced structurally.** The `.orig` is
      never touched (`fact_note_path` can only ever yield a `.md`, and
      `edit_fact` is the only write); the file is never renamed (the name
      encodes the timestamp + slug that goals' `demonstrated_by` wikilinks
      point at, so retitling changes frontmatter only); `timestamp` and
      `attachment` are read back and written straight through. `description`
      is regenerated from `summary` via `okf::description_from_summary` and is
      not a client-settable field. `value` is validated to `1..=5`, the same
      range `prompt::parse_fact_result` enforces.
- [x] **Facts *are* deletable** (`.md` + `.orig` + attachment bytes), by
      explicit user decision after the alternative (edit-only, on the strength
      of the `.orig` guarantee) was put to them. Inbound `demonstrated_by`
      links are left dangling -- a fact can't hold a backlink and can't be
      partially unlinked the way a goal can -- but never silently: the confirm
      dialog names the goals beforehand and the worker's reply names them
      after. This is the one place kamaji discards a raw message.
- [x] **A fact's identity rides in the query string**
      (`PATCH|DELETE /api/facts?target=...`), never a path segment: it
      contains `/`, and `%2F`-in-a-segment round-tripping is not something to
      bet a path-traversal boundary on. `fact_note_path` validates it
      structurally (four segments, month name from `chrono`, stem must be a
      component `Path::file_name` returns verbatim).
- [x] **UI**: one page, three tabs, one `itemDate`/`itemId` accessor pair
      shared by all three domains. The completion ring is scoped to
      todos/goals; facts show count + average value instead, never a fake 0/N.
      Fixed a pre-existing bug while there: inline edit boxes carried
      `autofocus`, which the browser only honours at parse time, so an editor
      Preact inserted into a rendered list opened unfocused -- `useAutoFocus`
      replaces it, which is what makes the keyed-diffing caret guarantee
      actually observable.
- [ ] **Not done in this pass**: no way to *create* a fact from the web UI.
      A fact's title/summary/value/slug come from the agent and its `.orig`
      preserves the raw message verbatim; a web form has neither to offer, and
      inventing them would break the one property `/fact` exists to hold.
      `/fact` in chat stays the only way to mint one.
- [ ] **Not done in this pass**: no way to add or remove a todo->goal or
      goal->fact link from the web UI. `/todo link` and `/demonstrate` remain
      the only writers, which is also why a linked goal can only be deleted
      after unlinking in chat.

### Newly captured, not designed yet

- [ ] **Module-level doc comments.** None of the `kamaji-core/src/*.rs` files
      (`agent.rs`, `claude.rs`, `codex.rs`, `worker.rs`, `routing.rs`, etc.)
      carry a `//!` module summary today. Add one to each describing what the
      module owns, at the level CLAUDE.md's Stack section already does in
      prose — helps orientation without duplicating the per-function doc
      comments that already exist in places.
- [ ] **Per-agent `UsageInfo` instead of one shared shape.** `UsageInfo`
      (`kamaji-core/src/prompt.rs:74`) is Claude-CLI-shaped
      (`input_tokens`/`output_tokens`/`cache_creation_input_tokens`/
      `cache_read_input_tokens`), and `codex.rs`'s `parse_codex_jsonl`
      (`:104-122`) already has to approximate into it — Codex's own
      `{input_tokens, cached_input_tokens, output_tokens}` doesn't line up
      1:1, so `cache_creation_input_tokens` is hardcoded to `0` and the base
      `input_tokens` is back-derived by subtraction. Consider a per-flavor
      usage type (mirroring the `claude.rs`/`codex.rs` peer split for the
      envelope itself) instead of forcing every agent through one Claude-
      shaped struct. Needs a decision on what `TokenUsage`/`/history`
      (downstream of `prompt::extract_tokens`) do with fields a given flavor
      can't populate.
- [ ] **`/history` shows stale/wrong information — bug, not yet diagnosed.**
      `cmd_history` (`kamaji-core/src/commands.rs:152`) calls
      `history::query_recent` (`kamaji-core/src/history.rs:74`), which reads
      the `JOB_HISTORY` table, sorts by `job_id` descending, and truncates to
      the limit — that logic looks correct on inspection, so the root cause
      (stale entries never overwritten, `log_job` not called on some path,
      wrong table, something else) isn't confirmed yet. Needs a repro plus a
      look at every `history::log_job` call site in `worker.rs` before
      deciding the fix.
- [ ] **`/history` should show which agent ran each job.** Right now
      `cmd_history`'s per-line output (`kamaji-core/src/commands.rs:179-202`)
      has no agent column — add one showing the `AgentFlavor` (`claude`/
      `codex`) for jobs that invoked an agent, and `n/a` for commands that
      don't call one at all (e.g. `/status`, `/todo`, `/goal`). Needs the
      flavor to actually be threaded into `JobHistoryRecord`
      (`kamaji-core/src/history.rs`) at `log_job` time, not just displayed —
      it isn't captured there today.

## Closed

### `/ingest` command

- [x] ~~Add an `ingest` entry to `COMMANDS` (`src/commands.rs`) taking one
      argument: a URL or freeform text.~~ — added, `mode: CommandMode::Queued`.
- [x] ~~`mode: CommandMode::Queued` — this touches Claude and the notes git
      repo, so it must go through `Queue::enqueue` and the single sequential
      worker like a normal ingest job, not `Sync`.~~
- [x] ~~Two distinct branches, not one payload type~~ — `route_ingest_command`
      (`src/worker.rs`) decides based on `urls::extract_urls`:
      - Arg looks like a URL → reuses `JobKind::Ingest`'s existing
        `process_ingest`, i.e. exactly the link-fetch + summarize + note +
        commit + push pipeline (same as the no-command path today).
      - Arg is freeform text → **not** filed as a note. `process_agent_query`
        calls the new `claude::run_agent_query`, which sends the text to
        Claude with a freeform (non-JSON-schema) prompt, and relays the
        reply straight back to Telegram.
- [x] ~~The no-command (default) path is explicitly **not** changing~~ —
      `process_ingest` in `src/worker.rs` is untouched; plain text with no
      leading `/` still always files a note. Agent-passthrough is only
      reachable via `/ingest <text>`.
- [x] ~~No args (`/ingest` alone) is a usage error, not an empty job~~ —
      `routing::handle_update` replies with `commands::INGEST_USAGE` and
      returns before enqueueing, mirroring the unknown-command reply.
- [x] ~~Routing rule stays intact~~ — `/ingest` is a known `Queued` command
      dispatched explicitly by name; the no-command-means-ingest rule and
      the unknown-command error path are unchanged.
- [x] ~~Tests~~ — `route_ingest_command` (`src/worker.rs`) covers the link
      path (bare URL and text-with-a-URL) and the agent-passthrough path.
      The "no args → usage reply, no enqueue" guard mirrors the
      already-untested unknown-command guard in `routing::handle_update` (no
      existing test in this codebase exercises that function's
      Telegram-calling side effects, only the pure `route_message`), so it's
      covered by the same boundary rather than a new network-mocking harness.

### Categories for notes

- [x] ~~Put in job history chached tokens, not just the aggregated tokens and
      the difference.~~ — `TokenUsage` (`src/claude.rs`) now carries
      `cache_creation`/`cache_read` alongside the existing aggregate `input`,
      populated from `UsageInfo`'s cache fields instead of being collapsed
      away. `#[serde(default)]` keeps old `job_history` redb entries
      deserializing. `/history` (`src/commands.rs`) renders the breakdown.
- [x] ~~Add a `category` field to the ingest result (alongside the existing
      freeform `tags`), returned by Claude's ingest prompt as part of the
      strict JSON contract.~~ — `IngestResult` (`src/claude.rs`) now carries
      `category`.
- [x] ~~Category maps to a folder: notes move from the current flat
      `notes/` layout (see `src/notes.rs`) to `notes/<category>/`.~~ —
      `write_note` writes under `notes/<category>/`.
- [x] ~~Before assigning a category, enumerate existing category folders under
      `notes/` **in Rust** (filesystem is already local to the daemon) and
      interpolate that list into the existing single ingest prompt, asking
      for `category` in the same strict-JSON response as `title`/`tags`/etc.~~
      — `notes::list_categories` enumerates them, `worker::process_ingest`
      hands the list to `claude::build_prompt`.
- [x] ~~Fallback when no existing category fits: allow Claude to create a new
      category folder freely, no artificial cap and no "uncategorized"
      catch-all bucket.~~ — the prompt explicitly says to reuse an existing
      category if it fits or name a new one otherwise, no cap, no catch-all.
- [x] ~~Update note frontmatter to include `category` and update the existing
      `write_note` tests in `src/notes.rs` for the new path layout.~~ —
      frontmatter has a `category` line; tests cover the new path layout and
      `list_categories`.
- [x] ~~Consider whether a Claude Code skill should own the categorization
      step~~ — decided against for now. Skills earn their complexity when
      Claude needs to autonomously decide when to invoke a workflow or use
      tools to discover state itself; here the state (existing folders) is
      trivial for Rust to gather up front, so folding it into the existing
      ingest prompt is simpler and keeps the strict-JSON contract easier to
      guarantee. Revisit if categorization logic grows richer later (e.g.
      merging/renaming categories over time).

### `/fact` command (bitacora / bio-log)

- [x] ~~Add a `fact` entry to `COMMANDS` (`src/commands.rs`), `mode:
      CommandMode::Queued` (invokes Claude and always touches the notes git
      repo, same reasoning as `/ingest`). `FACT_USAGE` mirrors
      `INGEST_USAGE`: no description *and* no attachment is a usage error,
      replied to immediately without enqueueing
      (`routing::handle_update`).~~
- [x] ~~Unlike `/ingest`, there is no agent-passthrough branch -- every
      `/fact <description>` call writes a bitacora entry, whether or not it
      contains a link.~~
- [x] ~~Telegram attachments: the bot previously only ever read
      `msg.text()`, so a message with a document and a caption (how
      `/fact ... ` + a file arrives) was silently dropped. `routing.rs` now
      reads `msg.text().or_else(|| msg.caption())` and captures
      `msg.document()`, carried on `JobKind::Command` as a new
      `attachment: Option<CommandAttachment>` field (`src/queue.rs`,
      `#[serde(default)]` so old queued payloads keep deserializing). The
      default no-command ingest path is unchanged -- a document with no `/`
      command in its caption still just ingests the caption text, attachment
      ignored, exactly as before this existed.~~
- [x] ~~Download support (`src/attachment.rs`): resolves the Telegram
      `file_id` via `getFile` then downloads the bytes, both steps under a
      new `TELEGRAM_FILE_TIMEOUT_SECS` timeout (default 30s) and a
      `MAX_ATTACHMENT_BYTES` cap (default 20MB) -- the "timeout every
      external call" convention applied to a new external call. A download
      failure (expired file, network error, over the cap) is logged and
      degrades to filing the fact without the attachment rather than failing
      the whole job.~~
- [x] ~~**Attachment text extraction is explicitly NOT in this pass** (user
      call: "for now just download files, text extraction in a second
      round"). The file's bytes are saved to disk next to the note, and
      Claude is told the attachment's filename so it can reference it, but
      is explicitly instructed not to guess its contents. Follow-up: parse
      md/txt/html directly and add a PDF-to-text crate, then feed extracted
      content into `claude::build_fact_prompt` the same way fetched URL
      content is today.~~
- [x] ~~Storage layout (`src/bitacora.rs`): decided
      `bitacora/<YYYY>/<Month>/` (full month name, e.g. `July`) rather than
      also nesting a `Q<n>` folder -- quarter is trivially derivable from
      month when generating the quarterly report, so a redundant quarter
      folder isn't worth keeping in sync. Filename is
      `<YYYYMMDD-HHMMSS>-<slug>`, numeric-suffixed on same-second collision
      (mirrors `notes::unique_path`).~~
- [x] ~~Two files per entry, always: `<base>.md` (rendered frontmatter +
      summary) and `<base>.orig` (the raw message text, saved verbatim, so
      nothing Claude summarized away is ever lost) -- plus a third,
      `<base>-<sanitized-filename>`, when an attachment was downloaded.
      Frontmatter is `title`, `date`, `value`, `tags`, and `attachment` (the
      saved filename) when present.~~
- [x] ~~`value` (1-5 integer, same shape as ingest's `importance`) replaces
      `category`/`source_url`: bitacora entries aren't filed by topic, and
      `value` is what the prompt asks Claude to score for a future quarterly
      self-review. New `claude::FactResult` / `claude::run_fact_prompt` /
      `claude::build_fact_prompt`, parallel to but separate from
      `IngestResult`/`run_ingest_prompt`/`build_prompt` -- shared only the
      fetched-URL-formatting helper (`format_fetched_urls`).~~
- [x] ~~`git::commit_and_push` generalized from a single path to
      `&[PathBuf]` plus a caller-supplied commit message (was
      hardcoded `"Add note: {title}"`) -- a fact commits 2-3 files
      (`.md`/`.orig`/attachment) together, not one file per commit.
      `/ingest` call site updated to pass a one-element slice and its own
      `"Add note: ..."` message.~~
- [x] ~~Path-traversal guard on the attachment filename
      (`bitacora::sanitize_filename`): Telegram-supplied filenames are
      untrusted input (unlike `slug`, which comes from Claude under an
      explicit filename-safe prompt instruction), so a crafted name like
      `../../.ssh/authorized_keys` is reduced to just its
      `Path::file_name()` before being joined into the bitacora month
      directory.~~

Remaining open items (attachment text extraction, quarterly report
generation) are tracked under Open → "`/fact` command (bitacora / bio-log) —
remaining".

### `/todo add`: extract tags without stripping them from the text

Today `parse_tag_token` (`kamaji-core/src/todo.rs`) treats a recognized
`#tag` token as consumed: it's pulled into `tags: Vec<String>` and dropped
from `text_words`, so `text` is reassembled from only the non-tag words (see
`parse_command_add_with_tags_interspersed_in_the_text`). `/todo add #work
finish the report #urgent` yields `text: "finish the report"` — the tag
words vanish from the sentence they were typed in. Tags should be *extracted
into structured data* without *removing* them from the narrative: the stored
text is what the user actually wrote, and the `#tag` mentions are part of
that narrative, not just metadata to be pulled out.

- [x] ~~Change `parse_command`'s `"add"` branch so `text` is the original
      input joined verbatim, tag tokens included, while `tags` is
      still populated from the same scan (a token can be *recognized as* a
      tag without being *removed from* the text).~~ — fixed in
      `checklist::parse_command`'s `"add"` branch (`kamaji-core/src/checklist.rs`),
      shared by both `/todo` and `/goal`.
- [x] ~~`render_line`/`format_list` will then show tag words twice — once in
      the `[tags]` bracket, once inline in `text` — confirm that's the
      intended display rather than treating it as a regression to hide.~~ —
      confirmed intended (user call); no dedup logic added.
- [x] ~~Update the tests that currently assert tags are stripped
      (`parse_command_add_with_tags_at_the_end`,
      `parse_command_add_with_tags_interspersed_in_the_text`) to assert the
      new behavior instead: `tags` populated as before, `text` equal to the
      original joined input, `#tag` substrings included.~~ — updated in
      `kamaji-core/src/todo.rs`.
- [x] ~~Land this before `/goal` is implemented below — its spec says "same
      tag mechanism as `/todo`", so fixing the stripping behavior here first
      avoids copying the bug into `goal.rs`/`checklist.rs`.~~ — done as one
      combined change (see `/goal` section below).
- [x] ~~**Open cross-reference (raised by the ingest-tags section below):**
      `parse_tag_token` (`kamaji-core/src/checklist.rs:147`) is private to
      `checklist.rs` and recognizes *whole whitespace tokens only*, so a tag
      followed by punctuation (`#work,`) is silently not a tag here either.
      Two decisions land on this function when ingest starts using it: making
      it reachable (`pub(crate)`, or a shared `tags` module), and whether the
      punctuation trim applies to `/todo`/`/goal` as well — sharing it would
      change already-shipped behavior for these commands. Decide there, but
      the blast radius is here.~~ — resolved by extracting a dedicated
      `kamaji-core/src/tags.rs` module (`parse_tag_token`, `extract_tags`,
      `merge_user_tags`), so `checklist.rs` no longer owns these; the
      punctuation trim is shared, per the ingest-tags section below.

### `/goal` command (align TODOs to goals)

Same tag mechanism as `/todo` (`#tag1 #tag2`), but for longer-lived
objectives rather than one-off action items. A later phase runs an
alignment pass over open TODOs and open goals — tag overlap first, an
explicit reference id later — see "Goal/TODO alignment" under Open.

- [x] ~~Add `goal` to `COMMANDS` (`kamaji-core/src/commands.rs`), `mode:
      CommandMode::Queued` — same reasoning as `/todo`: it writes to the
      notes git repo, so it goes through `Queue::enqueue` and the single
      sequential worker, not `Sync`.~~
- [x] ~~Subcommands mirror `/todo`, not reinvented: `/goal add <text> #tag1
      #tag2 ...`, `/goal list [open|close]`, `/goal achieve <id>` —
      `achieve` instead of `resolve` because "resolved" reads oddly for a
      goal, but the state machine (open → closed-with-timestamp) is
      identical to `TodoStatus`.~~
- [x] ~~**Design decision before writing code:** `todo.rs` (726 lines) is
      almost entirely generic "id-tagged checklist stored as dated
      markdown files" logic (`LINE_RE`, `next_id`, `todo_files`,
      `add_entry`, `list_entries`, `resolve_entry`, `render_line`/
      `parse_line`) with only the folder name (`todo/`) and the
      closed-state verb (`resolved`) varying. Copy-pasting that file to
      `goal.rs` with `s/todo/goal/` duplicates ~700 lines for the same
      data structure twice. Recommend extracting the generic parts into a
      shared module (e.g. `checklist.rs`) parameterized by a small
      config (folder name, closed-state verb), with `todo.rs`/`goal.rs`
      becoming thin wrappers supplying that config plus their own
      `TodoAction`/`GoalAction` parse types. Do this refactor *as part
      of* adding `/goal`, not after — retrofitting it once two full
      copies exist is more work and risks the two drifting apart first.~~
      — done: `kamaji-core/src/checklist.rs` holds the generic engine
      (`Config`, `Action`, `Entry`, `Status`, `CloseOutcome`,
      `parse_command`, `add_entry`, `list_entries`, `close_entry`,
      `format_list`); `todo.rs`/`goal.rs` are thin wrappers supplying
      their own `Config` const plus `TodoAction`/`GoalAction`. Shared
      error type `ChecklistError` (renamed from `TodoError`) in
      `error.rs`.
- [x] ~~Storage: `goals/<YYYY>/<MM>-goals.md`, same line shape as
      `todo.rs`'s `render_line`: `- [ ] #<id> <created> [<tags>] <text>`,
      closed adds ` (achieved <timestamp>)`.~~ — superseded: the `#<id>`
      token and the id counter are gone (see "id-less reference format"
      further down this section).
- [x] ~~`GOAL_USAGE` const mirrors the `INGEST_USAGE`/`FACT_USAGE` pattern
      — `/goal` alone is a usage error, replied to immediately without
      enqueueing (`telegram::handle_update`/`matrix::handle_message`).~~
      — no separate const: `/goal`, like `/todo`, has three subcommands
      each with their own usage shape, so `kamajid/src/transport/mod.rs`'s
      `dispatch_routed_job` delegates to `goal::parse_command(args)` for
      the "no args"/bad-subcommand case, mirroring how `/todo` is already
      wired (rather than a single-flag check like `/ingest`/`/fact`'s
      constants, which only ever take one flat argument). Same
      user-facing behavior, no redundant constant.
- [x] ~~Tests: parity with `todo.rs`'s test module — `parse_command`
      coverage per subcommand, add/list/achieve round-trip, id counter
      across months, idempotent achieve, not-found error.~~ — the generic
      engine's full test suite lives in `checklist.rs` (including the
      tag-in-text fix); `goal.rs`/`todo.rs` each keep a thin parity suite
      proving their own `Config` wiring end to end.

### `/todo`/`/goal`: id-less reference format (`EntryKey`)

`/todo resolve <id>`/`/goal achieve <id>` scanned every `todo/**/*.md` (or
`goals/**/*.md`) file, oldest-first, to find a numeric id — worst case on
the most common target, since a just-added (highest-id) entry lives in the
newest file, scanned last. `/todo add`/`/goal add` scanned everything too,
just to compute the next id (`checklist::next_id`). Both costs grew with
total historical entries. Separately, the numeric id was fragile for
manual editing in Obsidian: hand-adding a line meant picking the right
next id yourself, with no validation against collisions.

- [x] ~~Drop the stored id entirely. An entry's identity is now
      `EntryKey { date, line }` (`checklist::EntryKey`), rendered/parsed as
      `YYYY-MM-DD-N` (e.g. `2026-08-03-2`, `line` = 1-based position among
      checklist items under that day's heading) — derived from *where* the
      entry sits, not a separately-tracked counter. Given a key, locating
      an entry is direct addressing (open one year/month file, jump to one
      day heading) instead of a scan across every file ever written.
      `add`/`add_entry` no longer scan anything cross-file either, since
      there's no global counter left to compute.~~
- [x] ~~Backward compatible with every existing `#<id> [tags] text` line,
      no migration/rewrite of already-committed files: `line_format`'s
      parse regex tolerates (and discards) whatever single token, if any,
      sits between the checkbox and `[tags]` — an old `#7 ` token and a
      fresh id-less line both parse identically.~~
- [x] ~~Added a symmetric `reopen` action (`/todo reopen`, `/goal reopen`)
      alongside the existing one-directional `resolve`/`achieve` —
      `checklist::Action::Reopen`, `open_entry`, `Config::reopen_subcommand`.~~
- [x] ~~`/todo resolve|reopen`/`/goal achieve|reopen` accept either the
      full `EntryKey` or a plain shorthand number from the most recently
      shown `/todo list`/`/goal list` **in that chat/room**
      (`checklist::EntryReference::{Key, Shorthand}`). Shorthand resolution
      is backed by a new small redb cache, `checklist::cache::ChecklistCache`
      (table `CHECKLIST_LIST_CACHE` in `db.rs`, keyed by
      `"{domain}:{chat_ref}"`) — `list` overwrites the cached order for
      that domain+chat, `resolve`/`reopen` look it up when given a bare
      number. A miss (no recent list, or out of range) is a friendly reply
      ("run `/todo list` first, or use the full key"), not an error.~~
- [x] ~~Files: `checklist/mod.rs` (`EntryKey`, `EntryReference`, `Action::Reopen`,
      `set_status` replacing `close_entry`'s cross-file scan), `checklist/
      line_format.rs` (permissive regex, id-less render), `checklist/
      cache.rs` (new), `db.rs`/`state.rs`/`kamajid/src/main.rs` (new cache
      table + `AppState` field), `todo.rs`/`goal.rs` (mirrored wrapper
      changes), `worker/todo_job.rs`/`goal_job.rs` (take `chat: &ChatRef`,
      new `Reopen` arm, reply text uses the key instead of `#{id}`).~~

### `kamajid` transport defaults (Telegram is a leftover default, not a real one)

`kamajid/src/main.rs` and `kamaji-core/src/config.rs` still treat Telegram
the way the very first implementation did, before Matrix/REST/socket
existed: as *the* transport, not *a* transport. In practice REST and the
Unix socket (`kamaji` CLI) are what should be running by default —
Telegram (like Matrix) is one chat-platform integration among several,
and should be equally opt-in. Two concrete leftovers, not just a docs
mismatch:

- [x] ~~`Config::bot_token` (`kamaji-core/src/config.rs`) is a bare
      `String`, `.expect()`'d out of `TELEGRAM_BOT_TOKEN` unconditionally
      — unlike `matrix`/`rest_api`, which are `Option<...>` and only
      `.expect()` their sub-fields *after* their own gate env var
      (`MATRIX_HOMESERVER_URL`/`REST_API_BIND`) is present. Telegram now
      follows the same `telegram_config_from_env() -> Option<TelegramConfig>`
      pattern, gated on `TELEGRAM_BOT_TOKEN` being set; `allowed_chats`/
      `ALLOWED_CHAT_IDS` are folded into that same struct/gate.~~
- [x] ~~`kamajid/src/main.rs` connected to Telegram unconditionally at
      startup (`Bot::new`, `bot.get_me().await?` — a hard failure if
      unreachable or misconfigured, `bot.set_my_commands(...)`) instead
      of building `Option<TelegramClient>` the way `matrix` is already
      built conditionally a few lines below it. Now gated on
      `config.telegram` the same way.~~
- [x] ~~The real structural issue: `transport::telegram::run(bot, state,
      poll_watchdog_timeout).await` was the **last statement in `main()`**
      — it's what kept the whole process alive, while
      `transport::matrix::run`/`transport::rest::run`/
      `transport::socket::run` were just `tokio::spawn`ed background
      tasks that no-op if unconfigured, so a dead subsystem went
      unnoticed. Fixed: all transports (plus the worker and socket) are
      now spawned into one `tokio::task::JoinSet`, gated per-transport on
      whether they're configured; `main()` blocks on `tasks.join_next()`
      and treats any exit or panic as fatal, so systemd's
      `Restart=always` is what recovers a dead subsystem instead of the
      daemon quietly running half-alive.~~
- [x] ~~Once Telegram can be fully absent, decide what "REST and Unix
      Socket are the defaults" means concretely for REST: today
      `rest_api: Option<RestApiConfig>` no-ops silently
      (`transport::rest::run` returns early) unless `REST_API_BIND` is
      explicitly set — same opt-in posture as Matrix. Decided: REST stays
      opt-in via `REST_API_BIND` (no default bind address) — a default
      would force `REST_API_TOTP_SECRET` on every deployment, including
      the currently-running production instance, which has neither set
      today.~~
- [x] ~~Guardrail to decide explicitly, not by accident: what happens when
      Telegram, Matrix, *and* REST are all unconfigured? Unix socket
      alone means kamajid only accepts local CLI connections. Decided:
      `Config::from_env()` fails fast at startup in this case (an
      `assert!` requiring at least one of `TELEGRAM_BOT_TOKEN`,
      `MATRIX_HOMESERVER_URL`, or `REST_API_BIND`) rather than silently
      running in a CLI-only mode nobody asked for.~~
- [x] ~~Update anything documenting `TELEGRAM_BOT_TOKEN` as required
      (README, `docs/`, `deploy/kamaji.service` — note the local
      uncommitted change there already drops the placeholder token line,
      part of the unrelated §1.4 secrets-hardening item under VM
      hardening) once it's actually optional in code. README's "Required
      environment" section now only lists `NOTES_REPO_PATH`/`REDB_PATH`;
      Telegram moved to "Optional" alongside Matrix/REST, with the
      fail-fast behavior documented.~~

### Push rejected because remote is ahead → rebase and retry

`git::commit_and_push` (`kamaji-core/src/git.rs`) today treats every push
failure identically: retry `git push` verbatim with exponential backoff, then
give up and return `PushOutcome::CommittedNotPushed`. That's right for a
transient network/auth blip, but wrong for the common case — the notes repo was
edited and pushed from somewhere else (laptop, GitHub web), so `origin` is
ahead and the push is *rejected as non-fast-forward*. Retrying that identical
push N times can never succeed; it just burns ~30s of the single worker's time
before telling the user the note didn't push. It should instead integrate the
remote commits (`pull --rebase`) and push again.

- [x] ~~**Only rebase on an actual non-fast-forward rejection, not on any push
      failure.**~~ — `classify_and_maybe_rebase` (`kamaji-core/src/git.rs`)
      runs `git fetch` then `git rev-list --count HEAD..@{u}` after a failed
      push; anything that can't be classified this way (fetch unreachable, no
      upstream) falls back to the original blind-retry-with-backoff path
      unchanged. No stderr pattern-matching.
- [x] ~~**Rebase at most once per `commit_and_push` call, then push once
      more.**~~ — guarded by a `rebased` one-shot flag in the retry loop; a
      successful rebase retries the push immediately (no backoff sleep,
      since real progress was made), a conflict or dirty tree returns
      `CommittedNotPushed` right away rather than looping.
- [x] ~~**Never `--force`, never `--force-with-lease`, never `git reset`.**~~ —
      not present anywhere in `git.rs`; the rebase replays our commit on top
      of the remote's via plain `git pull --rebase`.
- [x] ~~**Guardrail: the repo must never be left mid-rebase.**~~ — on any
      `git pull --rebase` failure, `classify_and_maybe_rebase` runs
      `git rebase --abort` unconditionally before returning `Conflict`,
      commented with *why* (shared working directory, poisons every
      subsequent job otherwise). Covered by
      `remote_ahead_conflicting_change_leaves_clean_worktree`.
- [x] ~~**Decide the dirty-worktree policy explicitly.**~~ — decided:
      detect-dirty and skip the rebase entirely, reporting
      `CommittedNotPushed { reason: NotPushedReason::RemoteAheadDirtyTree }`
      ("pull and rebase by hand"). No `--autostash`. Follow-up fix: "dirty"
      is `git status --porcelain --untracked-files=no` — the first cut
      omitted the flag, and since untracked files don't stop
      `git pull --rebase` but *do* show in `--porcelain`, one stray untracked
      file (e.g. a note left behind by a job whose commit errored) silently
      disabled this whole path forever. Regression test:
      `untracked_file_does_not_count_as_dirty`, plus
      `uncommitted_tracked_change_skips_rebase` pinning the intended policy
      from the other side.
- [x] ~~**`tokio::time::timeout` on the new `fetch`/`rebase`/`push`
      invocations**~~ — all new subcommands (`fetch`, `rev_list`, `status`,
      `pull_rebase`, `rebase_abort`) go through the existing `run_git`/
      `run_git_stdout` helpers, so they get the same timeout and
      `GitError::{Timeout, CommandFailed}` handling for free.
- [x] ~~**Report the rebase to the user.**~~ — went with a new
      `PushOutcome::PushedAfterRebase` variant (success case, kept separate
      from `CommittedNotPushed`) plus a `NotPushedReason` enum
      (`ExhaustedRetries`/`RemoteAheadDirtyTree`/`RebaseConflict`) with a
      `Display` impl on `CommittedNotPushed`'s new `reason` field. All 6
      `PushOutcome` match sites in `kamaji-core/src/worker.rs` now go through
      one shared `describe_push_outcome` helper instead of duplicating the
      match arms.
- [x] ~~**Tests (`git.rs` has none today).**~~ — three hermetic tests added
      using `tempfile` + a local bare repo, no network: (a)
      `remote_ahead_non_conflicting_rebases_and_pushes`; (b)
      `remote_ahead_conflicting_change_leaves_clean_worktree` (asserts no
      `.git/rebase-merge`/`rebase-apply` and a clean `git status` after the
      abort); (c) `non_rebase_related_push_failure_behaves_as_before` (bogus
      remote URL, no rebase attempted).

### User `#tags` on ingest (from the message, never from fetched link content)

`/todo add` and `/goal add` already let the user tag an entry inline — any
`#tag` token anywhere in the args is recognized by `parse_tag_token`
(now `kamaji-core/src/tags.rs`, moved there from `checklist.rs` — see the
cross-reference item in the `/todo add` section above). Ingest has no such
thing: a note's `tags` come **only** from Claude's strict-JSON response
(`IngestResult.tags`, `kamaji-core/src/claude.rs:16`), so there's no way to
say "file this under `#rust`" when sending a link. This item gives ingest
the same inline-tag affordance, with one hard boundary: tags are read from
**the text the user sent kamaji**, never from text fetched from the links
in it.

- [x] ~~**Extend the same affordance to `/fact`.**~~ — `process_fact_command`
      (`kamaji-core/src/worker.rs`) now computes `tags::extract_tags(&raw_text)`
      before fetching any URLs in the message, same as `process_ingest`, and
      unions it into `fact_result.tags` via `tags::merge_user_tags` right
      after the Claude call returns, before `bitacora::write_fact`. The
      `/ingest`-only scope note below predates this and no longer applies.

- [x] ~~**The boundary is the whole point: parse tags from `raw_text` only.**~~
      — `process_ingest` (`kamaji-core/src/worker.rs`) computes
      `checklist::extract_tags(raw_text)` as its first line, before any URL
      fetch runs and without ever passing `FetchedContent` to it. Pinned by
      `worker::tests::extract_tags_never_sees_fetched_body_noise`, which
      shows the same recognizer misreading `#hashtag`/`#define`/CSS-selector
      noise if it were ever fed a fetched body — the reason the call site
      matters, not just the recognizer's rules.
- [x] ~~**Reuse `parse_tag_token`, don't reimplement it.**~~ —
      `parse_tag_token` (`kamaji-core/src/checklist.rs`) is now
      `pub(crate)`, plus a new `pub(crate) fn extract_tags(text: &str)`
      wrapping it (whitespace-split, order preserved, duplicates kept).
      `checklist::parse_command`'s `add` branch and `worker::process_ingest`
      both call `extract_tags` — one recognizer, no drift.
- [x] ~~**Prose punctuation...**~~ — decided: **share the trim** with
      `/todo`/`/goal` rather than keep it ingest-only. `parse_tag_token` now
      trims a leading `(`/quote and trailing `,.;:!?)]}`/quote before
      recognizing the `#`. It only *widens* what counts as a tag (no shipped
      test relied on punctuated tokens being rejected), so already-typed
      `#work`-style tags are unaffected, and `#work,` silently not tagging
      was arguably a bug in `/todo` too. Reasoning recorded in
      `parse_tag_token`'s doc comment. `/help`'s reply now states the rule
      (`commands::cmd_help`).
- [x] ~~**Keep the `#tag` in `raw_text` verbatim.**~~ — untouched;
      `process_ingest` never mutates `raw_text`, only reads tags out of it.
- [x] ~~**Merge in Rust after the Claude call...**~~ — `process_ingest`
      unions `user_tags` into `ingest_result.tags` right after the Claude
      call returns, before `notes::write_note`; frontmatter, the OKF `tags`
      field, the confirmation reply, and `record_last_note` all read
      `ingest_result.tags` already, so mutating it in place was enough.
- [x] ~~**Decide the merge rule...**~~ — `merge_user_tags`
      (`kamaji-core/src/worker.rs`): union, user tags first (typed order),
      case-insensitive dedupe keeping the first-seen spelling (the user's).
      Not an override — both sides are additive signal per CLAUDE.md.
- [x] ~~**Edge case: a message that is *only* tags.**~~ — decided: reply
      "Nothing to ingest: message contains only tags." and write no note.
      `has_standalone_text` was generalized from URL-only stripping to a
      per-token check (neither a URL nor a recognized tag counts as
      standalone content), reused for both this new guard (`urls.is_empty()
      && !has_standalone_text(...)`, checked before any fetch) and the
      existing all-fetches-failed guard below it — one function, not a
      second differently-shaped path.
- [x] ~~**Routing is unaffected.**~~ — confirmed; nothing in this item
      touched `routing.rs`.
- [x] ~~**Scope: `process_ingest`.**~~ — confirmed at the time; `/ingest
      <text>` agent-passthrough was untouched, and `/fact` was untouched
      until the follow-up item above extended the same affordance to it.
- [x] ~~**Tests.**~~ — (a) `extract_tags_never_sees_fetched_body_noise`; (b)/(c)
      `merge_user_tags_puts_user_tags_first_in_typed_order`,
      `merge_user_tags_dedupes_case_insensitively_keeping_user_spelling`,
      `merge_user_tags_keeps_non_overlapping_tags_from_both_sides`; (d)
      `has_standalone_text_true_for_bare_number_hash` plus
      `checklist::parse_tag_token_bare_number_after_trim_is_still_not_a_tag`;
      (e) `has_standalone_text_false_for_tags_only_message`/
      `has_standalone_text_false_for_url_plus_tag_only` pin the condition
      the new early-return branches on. Frontmatter ordering itself is
      already covered by `notes::writes_note_with_expected_filename_and_frontmatter`
      (renders `result.tags` in order), composed with (b)'s ordering
      guarantee. New punctuation-trim coverage:
      `parse_tag_token_trims_trailing_prose_punctuation`,
      `parse_tag_token_trims_surrounding_parens_and_quotes`,
      `extract_tags_reads_prose_with_punctuation`.

### `KAMAJI_REMOTE_URL` env fallback: fix precedence, empty values, no tests

**The env var itself already exists** — `remote_url` (`kamaji/src/main.rs:98`)
falls back to `KAMAJI_REMOTE_URL` when `--remote` is absent, `socket_path`
(`:92`) does the same with `KAMAJI_SOCKET_PATH`, and both are documented
(`README.md:376`, `docs/remote-api.md:123`). So exporting it in a shell profile
works today; nothing to add. What's missing is everything around it: the
precedence between the two is backwards, an empty value is treated as a real
URL, and the client has **no test module at all**.

- [x] ~~An explicit `--socket` must beat an ambient `KAMAJI_REMOTE_URL`; today
      it silently loses.~~ — fixed: `remote_url` (`kamaji/src/main.rs`) now
      takes a `use_env_fallback: bool` that callers set to `cli.socket.is_none()`,
      so an explicit `--remote` flag still beats `--socket`, but the
      `KAMAJI_REMOTE_URL` *env* fallback is only consulted when `--socket` was
      not typed. Doc comments on both fields updated to state this.
- [x] ~~Decide whether there's an explicit "use the local socket" escape
      hatch.~~ — added a `--local` boolean flag: forces the local socket,
      ignoring both `--remote` and `KAMAJI_REMOTE_URL` unconditionally.
      Decided with the user in favor of a dedicated flag over an
      optional-value `--socket` (`Option<Option<PathBuf>>`) — simpler clap
      type, more discoverable in `--help`.
- [x] ~~`KAMAJI_REMOTE_URL=` (empty) must mean unset, not "a URL that is the
      empty string".~~ — added a shared `normalize()` helper (trims, then
      `filter(!is_empty())`) used by both `remote_url` and `socket_path`, for
      both the CLI flag and the env var.
- [x] ~~Validate the scheme once, at resolution, not per request.~~ — added
      `check_scheme`/`resolve_remote`: rejects non-`http(s)` with an error
      naming the source (`--remote` vs `KAMAJI_REMOTE_URL`) via `RemoteSource`,
      warns (doesn't reject) on plain `http`. Runs once in `main`, before any
      of the three `format!`/`send()` call sites.
- [x] ~~Tests: the client binary has no `mod tests` today.~~ — added, following
      `config.rs`'s `ENV_LOCK`/`clear_env`/`set_env` convention: 17 tests
      covering flag-beats-env, env-used-when-no-flag, the
      `--socket` + `KAMAJI_REMOTE_URL` regression case, empty/whitespace-only
      env, trailing-newline trimming, and the new scheme checks.
- [x] ~~Docs:~~ — `README.md` gained a "Transport resolution order" section
      stating the full flag > env > default precedence (including `--local`)
      once; `docs/remote-api.md` now points at it instead of paraphrasing.

### Agent backend abstraction (`agent.rs`): OpenShell as a pluggable runner, not a swap

CLAUDE.md's Stack section pins Claude invocation exactly: `tokio::process::Command`
→ `claude -p "..." --output-format json`, parsed with `serde_json`, never
`--dangerously-skip-permissions` — and says not to swap this without an explicit
request. This *is* that explicit request, but it's deliberately not a swap: the
user has [OpenShell](https://docs.nvidia.com/openshell/latest/about/overview.md)
(NVIDIA's sandboxed agent runtime — kernel-level isolation via Landlock,
declarative YAML policy across filesystem/network/process/inference) installed
and wants it as the *vehicle* that runs Claude (or another agent CLI — OpenCode,
Hermes — later), not a replacement for Claude itself. The ask is a generic
interface so kamaji can run an agent directly (today's behavior, unchanged) or
run it wrapped by OpenShell, and eventually swap which agent binary entirely,
without three call sites (`process_ingest`/`process_fact_command`/
`process_agent_query` in `kamaji-core/src/worker.rs`) each needing to know
which. `claude.rs` stays — this is an interim-compatibility layer, not a
removal.

**Landed shape, differs from this section's original framing below:** rather
than shelling out to the `openshell` CLI (`openshell sandbox exec -- <bin>
...`) as a subprocess, kamaji depends on `openshell-sdk`, NVIDIA's own async
gRPC client crate (`crates/openshell-sdk` in the `NVIDIA/OpenShell` repo,
unpublished — git/rev dependency), because that's how OpenShell's own TUI
talks to a gateway, and it gives typed `stdin`/`stdout`/`exit_code`/error
handling instead of subprocess-of-subprocess guessing. Sandbox lifecycle
(create/delete) is explicitly out of kamaji's scope — a sandbox is
provisioned once, out-of-band (see `docs/openshell.md`'s "Full rebuild"
pattern), and kamaji's `agent::Runner::OpenShell` only `exec`s into it plus
one startup `wait_ready` check. See the updated CLAUDE.md Stack section for
the landed env vars and dependency-exception writeup.

Two items from this effort (OpenShell auth beyond v1, pinned-dependency
maintenance) are still open — see Open → "Agent backend abstraction
(`agent.rs`) — remaining".

- [x] ~~**The seam already mostly exists — use it.** `invoke_claude`
      (`kamaji-core/src/claude.rs:90-129`) already isolates *all* of the
      spawn/stdin-pipe/timeout/envelope-parse mechanics from the Claude-specific
      parts (`build_prompt`/`build_fact_prompt`/`build_agent_prompt`'s prompt text,
      `IngestResult`/`FactResult`'s strict-JSON schemas). Moving/generalizing
      `invoke_claude` into a new `agent.rs` — parameterized over *how* to launch
      the child process rather than hardcoding `Command::new(claude_bin)` — is a
      mechanical extraction, not a rewrite. `claude.rs` keeps every prompt
      builder, both result schemas, and its own `run_ingest_prompt`/
      `run_fact_prompt`/`run_agent_query` — they just call into `agent.rs`'s
      runner instead of spawning directly.~~ — done: `agent::invoke(bin, args,
      stdin, timeout)` (`kamaji-core/src/agent.rs`) now owns the spawn/
      stdin-pipe/timeout/exit-status mechanics; `claude.rs`'s `invoke_claude`
      shrank to building the argv/stdin and parsing the returned stdout as a
      `ClaudeEnvelope`. No `Runner` enum yet (see next item) — only the
      mechanical extraction, direct-exec only.
- [x] ~~**Two independent axes — don't conflate them into one flag.** (a) *which
      agent* answers the prompt (Claude today; OpenCode/Hermes later) and (b)
      *how it's launched* (direct exec vs OpenShell-wrapped). Collapsing these
      into a single enum (e.g. `AgentBackend::Claude | OpenShellClaude |
      OpenCode`) will multiply awkwardly the moment a second agent needs both a
      direct and an OpenShell variant. Model as two orthogonal settings: a
      *runner* (direct | openshell) that only decides argv wrapping, and an
      *agent* (which binary + which envelope/schema contract) that stays
      `claude.rs`'s job as today, with a thin parallel module per additional
      agent later.~~ — done: `agent::Runner` (`kamaji-core/src/agent.rs`) is
      exactly this two-variant enum (`Direct | OpenShell { client,
      sandbox_name }`), threaded as a new leading parameter through
      `agent::invoke`/`claude::invoke_claude`/`run_ingest_prompt`/
      `run_fact_prompt`/`run_agent_query`. `claude.rs` is untouched in shape —
      still the only place that knows the argv (`["-p",
      "--output-format", "json"]`) and envelope/schema contract.
- [x] ~~**The envelope shape is Claude-CLI-specific — don't assume it's universal.**
      `ClaudeEnvelope`/`UsageInfo` (`claude.rs:62-74`) parse `claude -p
      --output-format json`'s exact JSON shape (`result`, `usage.input_tokens`,
      `usage.cache_creation_input_tokens`, ...). Nothing confirms OpenCode or
      Hermes emit the same fields under the same flags. `agent.rs`'s generic
      runner should return raw stdout/stderr/exit status; envelope parsing
      stays a per-agent concern (`claude.rs`'s today, a hypothetical
      `opencode.rs`'s later) rather than being hoisted into the "generic"
      layer and quietly assuming Claude's shape is the norm.~~ — done:
      `agent::invoke` returns `RawOutput { stdout, stderr, success }`, no
      JSON/envelope awareness at all; `ClaudeEnvelope` parsing stays entirely
      in `claude.rs`.
- [x] ~~**Confirm OpenShell passes stdin/stdout/exit code through transparently
      before relying on it.** The fetched docs describe the wrapping syntax
      (`openshell sandbox create -- claude ...`) but not stdin/stdout/exit-code
      semantics. This matters concretely here: the prompt is piped via stdin
      specifically to dodge `E2BIG` on oversized prompts (fetched URL content
      can exceed `MAX_ARG_STRLEN`, see `claude.rs:83-89` and the
      `invoke_claude_pipes_oversized_prompt_via_stdin` test) — if OpenShell
      buffers, truncates, or otherwise mishandles a large piped stdin, that
      regression comes back silently. Spike this against a real OpenShell
      install with an oversized prompt before wiring it into the worker path.~~
      — resolved by *not* shelling out to the CLI at all: kamaji uses the
      `openshell-sdk` gRPC client instead (see the section intro above),
      whose `ExecOptions.stdin: Option<Vec<u8>>` is a typed RPC field, not an
      OS pipe — the E2BIG-style truncation concern doesn't apply the same way.
      Concrete verification artifact: `agent::tests::openshell_smoke_test_oversized_stdin`
      (`#[ignore]`d, run manually with `cargo test -p kamaji-core --ignored --
      openshell_smoke_test` against a real gateway + pre-provisioned sandbox).
- [x] ~~**Config: generalize `claude_bin`, keep the opt-in pattern already used
      for Matrix/REST.** `Config::claude_bin` (`kamaji-core/src/config.rs:15`,
      env `CLAUDE_BIN`, default `"claude"`) becomes something like a runner
      selector (`AGENT_RUNNER=direct|openshell`, default `direct`) plus
      whatever OpenShell needs when selected (sandbox policy path/name, the
      `openshell` binary location). Unset `AGENT_RUNNER` (or it defaulting to
      `direct`) must mean byte-for-byte today's behavior — same "gate var
      absent → fully off" convention as `MATRIX_HOMESERVER_URL`/`REST_API_BIND`.~~
      — done, but not the sketched shape: no separate `AGENT_RUNNER` selector
      was added. `OpenShellConfig` (`kamaji-core/src/config.rs`) is gated on
      `OPENSHELL_GATEWAY_URL` alone (+ `OPENSHELL_SANDBOX_NAME`,
      `OPENSHELL_READY_TIMEOUT_SECS`), following the exact
      `*_config_from_env() -> Option<XConfig>` template Matrix/REST/Telegram
      already use — presence of the gate var *is* the selector, same as those
      three. `claude_bin`/`claude_timeout` were kept exactly as they were
      (they name the in-sandbox binary and its own per-call timeout
      regardless of runner), not folded into the new struct.
- [x] ~~**Error mapping across the sandbox boundary.** A policy that's too
      restrictive (e.g. blocks the network egress `claude` needs, or the git
      push path) will fail as an OpenShell/sandbox-level denial, not a normal
      `claude` exit code — `ClaudeError`'s variants (`error.rs:20-49`,
      `Spawn`/`NonZeroExit`/`Timeout`) need a place for "the sandbox itself
      rejected this" distinct from "the wrapped agent failed on its own
      terms," so a misconfigured policy doesn't get logged as if Claude itself
      broke.~~ — done: new `AgentError::SandboxRejected(#[source]
      openshell_sdk::SdkError)` variant (`kamaji-core/src/error.rs`). Costs no
      heuristic/stderr-matching — `openshell_sdk::OpenShellClient::exec` never
      returns `Err` for the wrapped binary's own non-zero exit (that's
      `ExecResult::exit_code`, mapped to the existing `NonZeroExit`), so the
      SDK's own `Result` boundary already draws the sandbox-vs-agent line;
      `agent::map_exec_result`/`map_exec_success` just mirror it. `ClaudeError`
      needed no change — it already wraps `AgentError` transparently.
- [x] ~~**`--dangerously-skip-permissions` guardrail carries over regardless of
      runner.** Sandbox isolation is a different, additive control, not a
      replacement for it — restate this explicitly wherever the OpenShell
      invocation is built, so it doesn't quietly get treated as "sandboxed
      now, so permissions flag is fine."~~ — comment restated at
      `claude.rs`'s `invoke_claude`, the one place (regardless of runner) that
      builds the `["-p", "--output-format", "json"]` argv both runners share
      unchanged.
- [x] ~~**Testing stays possible without OpenShell installed.** The existing fake-
      binary-standing-in-for-`claude` pattern (`invoke_claude_pipes_oversized_prompt_via_stdin`,
      `claude.rs:427-459`) must keep working against the generic runner in
      `direct` mode — don't design `agent.rs` so tightly around `openshell`
      that CI/dev environments without it installed lose test coverage on the
      spawn/stdin/timeout mechanics.~~ — done: the fake-binary E2BIG test moved
      to `agent::tests::invoke_pipes_oversized_stdin` (mechanics-level, no
      envelope), `claude.rs` keeps its own oversized-prompt test as an
      end-to-end check through `invoke_claude`; both use a plain shell script,
      no OpenShell dependency. Added `agent::tests::invoke_reports_non_zero_exit_with_stderr`
      alongside it.
- [x] ~~**Land the extraction before the OpenShell wiring.** Move
      `invoke_claude`'s mechanics into `agent.rs` as a behavior-preserving
      refactor first (direct-exec only, existing tests green, no config
      changes) — then add the `openshell` runner variant as a follow-up once
      the stdin/stdout spike above is confirmed. Keeps the risky unknown
      (OpenShell's process-wrapping semantics) isolated from the mechanical
      part (splitting a module).~~ — done, this pass: `cargo clippy
      --all-targets -- -D warnings`, `cargo fmt --check`, and the full
      workspace test suite (182 tests) all pass with zero config/behavior
      change. Remaining items (runner/agent axis, OpenShell wiring, config,
      error mapping, CLAUDE.md) were the follow-up, landed above.
- [x] ~~**CLAUDE.md needs updating once this lands**, not just code — the Stack
      section's "do not swap without an explicit request" line is exactly what
      this item is that request for; document the runner/agent split there so
      the next change to this area starts from the new shape, not the old
      single-binary assumption.~~ — done: Stack section's Claude-invocation
      bullet now describes `agent::Runner::Direct | OpenShell` as landed, the
      opt-in env vars, and a scoped-exception paragraph for the
      `openshell-sdk` git dependency (pin/bump process, C++-toolchain build
      requirement, inert `telemetry` feature pull-in), matching the existing
      matrix-sdk/sqlite exception's format.

### Rename `CLAUDE_*` leftovers to `AGENT_*` now that the agent axis is multi-flavor

The agent backend abstraction above (`AgentFlavor::Claude | Codex`, `AGENT_FLAVOR`)
already generalized the *binary selection* (`Config::agent_bin`, doc comment at
`kamaji-core/src/config.rs:30-37`), but several names alongside it still say
"Claude" even though they apply identically regardless of which flavor is
active — a naming leftover from before `codex.rs` existed, not a functional
bug. Concretely, grep-confirmed:

- `Config::claude_timeout` (`kamaji-core/src/config.rs:38`), env
  `CLAUDE_TIMEOUT_SECS` (`config.rs:213`) — bounds the agent subprocess/RPC
  timeout for *whichever* flavor is active (`worker.rs:294,341,357,414,522,540`
  all read `state.config.claude_timeout` regardless of `agent_flavor`;
  `kamajid/src/transport/mod.rs:211` sums it into a watchdog timeout the same
  way). The name is misleading the moment `AGENT_FLAVOR=codex` is set — it's
  really "the agent call timeout."
- Env `CLAUDE_BIN` (`config.rs:206`) — still the literal env var name for the
  Claude-flavor case per the doc comment at `config.rs:33-36` (`CODEX_BIN` is
  its Codex-flavor sibling); this one is arguably *correctly* named today
  since it's genuinely Claude-specific, but worth revisiting together with the
  rest so the env var surface reads consistently (`AGENT_FLAVOR`,
  `AGENT_TIMEOUT_SECS`, then per-flavor `CLAUDE_BIN`/`CODEX_BIN` underneath) rather
  than one generic name and one fossil.
- `claude_bin: &str` parameter name in `claude.rs:19,31` — cosmetic, but it's
  the same value `Config::agent_bin` resolved generically; reads oddly next to
  `codex.rs`'s equivalent.
- README (`:212-213,229,233`) and `deploy/kamaji.service` (`:17,25`) document
  `CLAUDE_BIN`/`CLAUDE_TIMEOUT_SECS` as if they were the only agent, predating
  the Codex flavor — needs updating in lockstep with whatever the code decides.

- [x] ~~**Is `CLAUDE_TIMEOUT_SECS` → `AGENT_TIMEOUT_SECS` a breaking env-var
      rename or does it need a deprecation shim?**~~ — went with the hard
      rename, no shim: `Config::claude_timeout` → `agent_timeout`
      (`kamaji-core/src/config.rs`), env `CLAUDE_TIMEOUT_SECS` →
      `AGENT_TIMEOUT_SECS`, updated in the same change across `worker.rs`'s six
      call sites, `kamajid/src/transport/mod.rs`'s watchdog-timeout sum,
      README, and `deploy/kamaji.service` (including its commented-out
      example line). Whoever deploys next must update their `EnvironmentFile`/
      `Environment=` line — flagged here since there's no shim to soften it.
- [x] ~~**Should `CLAUDE_BIN`/`CODEX_BIN` become `AGENT_BIN` too, or should
      per-flavor names stay?**~~ — left as-is per the recommendation:
      `CLAUDE_BIN`/`CODEX_BIN` and the `claude_bin`/`codex_bin` parameter names
      in `claude.rs`/`codex.rs` are genuinely per-flavor and already read as
      siblings of each other; only the flavor-agnostic timeout was renamed.
- [x] ~~**Sweep for the same pattern elsewhere before considering this done**~~
      — checked `config.rs:617`'s `from_env_agent_flavor_defaults_to_claude_with_claude_bin`:
      that name refers to the *value* `"claude"` (the default flavor) and the
      already-generic `agent_bin` field, not a misnamed timeout, so it's
      correctly named and left untouched. Full-repo grep for
      `CLAUDE_TIMEOUT_SECS`/`claude_timeout` after the rename returns nothing
      outside this TODO file.
- [x] ~~Update README (`:212-213,229,233`) and `deploy/kamaji.service`
      (`:17,25`) in the same change, not as a follow-up~~ — done: both files'
      `CLAUDE_TIMEOUT_SECS` mentions now read `AGENT_TIMEOUT_SECS`;
      `CLAUDE_BIN` mentions in both were left alone (see above).
      `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and the
      full workspace test suite (187 + 13 tests) all pass with no other
      changes needed.

### `ClaudeError` is now a shared multi-flavor error type wearing a Claude-specific name

Same class of leftover as the `CLAUDE_TIMEOUT_SECS`/`claude_timeout` rename
above, found by sweeping for the pattern afterward rather than designed up
front. `ClaudeError` was `prompt.rs`'s one shared error type returned by
*all three* entry points (`run_ingest_prompt`/`run_fact_prompt`/
`run_agent_query`) regardless of which `AgentFlavor` ran — both
`claude::invoke_claude` and `codex::invoke_codex`/`parse_codex_jsonl`
returned it, and it already carried a Codex-specific variant,
`CodexNoAgentMessage`, with a doc comment that said the quiet part
explicitly: *"Kept on this type (rather than a separate `CodexError`) so
`prompt.rs`'s entry points return one error type regardless of which
`AgentFlavor` ran."* That reasoning was sound — one shared error type is the
right call, matching how `AgentEnvelope` is already one shared success shape
— but the type's own *name* still said "Claude" the same way `claude_timeout`
used to.

- [x] ~~**Naming collision, decide first:** `AgentError` (`error.rs:26-70`) is
      already taken...~~ — renamed `ClaudeError` → `PromptError`
      (`kamaji-core/src/error.rs`), per the recommendation: short, matches
      "lives in the module that owns the strict-JSON contract," doesn't
      collide with `AgentEnvelope`/`AgentError`. Its doc comment now states
      explicitly that it's the envelope/schema layer on top of `AgentError`'s
      transport layer, shared by every `AgentFlavor`.
- [x] ~~**Variant names inside it are Claude-flavored too, same sweep
      needed:**~~ — `EnvelopeParse`, `MissingResult`,
      `ImportanceOutOfRange`/`ValueOutOfRange`'s `#[error("...")]` strings now
      say "agent output envelope"/"agent returned..." instead of "claude
      output envelope"/"claude returned...". `CodexNoAgentMessage`'s message
      was already flavor-correct (it's Codex-specific by definition) and its
      doc comment's "Kept on this type (rather than a separate `CodexError`)"
      justification was folded into the enum's own doc comment instead of
      repeated per-variant.
- [x] ~~**`SchemaParse`'s `source`/`raw` shape stays, only wording changes**~~
      — confirmed: no field/structure change, just the type name and message
      strings. Not persisted anywhere, so no `redb` compatibility concern.
- [x] ~~**Sweep call sites and test names once the type is renamed**~~ — every
      `ClaudeError` reference in `claude.rs`, `codex.rs`, and `prompt.rs`
      (imports, return types, constructors, and the one test assertion in
      `codex.rs`) renamed to `PromptError` in the same change; repo-wide grep
      after the rename confirms zero remaining `ClaudeError` references
      outside this TODO file's historical notes. Hard rename, no shim, same
      approach as the timeout rename — this type isn't persisted or part of
      any external contract, so there was nothing to keep compatible.
- [x] ~~**Do this one deliberately, not reflexively**~~ — confirmed still
      true: `claude_bin`/`CLAUDE_BIN` and the two `"claude invocation failed"`
      tracing log lines in `worker.rs:430,555` were deliberately left alone,
      out of scope for this item. `cargo fmt --check`, `cargo clippy
      --all-targets -- -D warnings`, and the full workspace test suite
      (187 + 13 tests) all pass with no other changes needed.

## Parked

Items here have been discussed but deliberately deferred: either the honest
answer is "probably not worth it" and no one has committed to closing them
outright, or the cost/benefit needs to sit for a while before deciding. Not
forgotten, just not next.

### Obscure the REST API's public paths (decide if it's worth it at all)

The three REST routes are hardcoded in `transport::rest::run`
(`kamajid/src/transport/rest.rs:75-84`) — `/auth/login`, `/api/cli`,
`/auth/logout` — and the `kamaji` client hardcodes the matching strings
(`kamaji/src/main.rs:179`, `:224`, `:283`). Since Caddy publishes the whole
subdomain on public HTTPS (`docs/remote-api.md` §3), `POST /auth/login` is
exactly the kind of path every commodity scanner wordlist already contains, so
the endpoint gets unsolicited traffic by default. **This item is a decision
first, an implementation only if the decision goes that way** — the honest
default answer is "not worth it, document why and close it".

- [ ] **Establish what the obscurity would actually buy, because the host name
      is not secret.** Caddy gets a Let's Encrypt cert for the subdomain, so it
      is published in Certificate Transparency logs and searchable (crt.sh) the
      moment it's issued. An attacker therefore always knows the host; the only
      unknown a secret path adds is the path itself. Meanwhile the real
      controls already in place are the ones doing the work: TOTP with a
      30-second step (`TOTP_STEP_SECS`, `kamaji-core/src/auth.rs:12`), each
      code single-use inside its own skew window via the in-memory last-step
      tracker (`auth.rs:28`, `verify_totp` at `:46`), and a per-source-IP rate
      limit of one request per 6s with burst 5 keyed on `X-Forwarded-For`
      (`rest.rs:62-73`). A scanner hitting `/auth/login` gets a 401 and eats
      the limiter. Conclusion to write down explicitly: the benefit is **log-
      noise reduction, not a security control** — do not let this item be
      recorded as "hardened the login endpoint".
- [ ] **First ask whether a gate is even the right tool. If the goal is log
      noise, filter the log.** A pre-auth gate reduces *reachability*; a Caddy
      `log` directive with a matcher reduces *log lines*. Those are different
      benefits, and per the bullet above the reachability one is worth close to
      nothing here. Doing nothing but quieting the log costs no secret, no
      client change, and no new failure mode — try that before anything below.
- [ ] **If a gate is built anyway: Caddy, not Rust, and a secret *header*, not
      a secret path.** Both forms are the same trick (a static shared secret
      checked before auth; `respond 404` otherwise, so a wrong/absent secret is
      indistinguishable from nothing being there) and both are Caddyfile-only
      on the server. The header is the better form, for the same reason
      RFC 6750 §2.3 deprecates bearer tokens in the URI: URLs leak. Specifically
      here — `run_remote`/`login` print the base URL on every failure
      (`kamaji/src/main.rs:188`, `:232`), a `--remote` argument is visible in
      `ps`, and the prefix would be baked into every doc example and shell
      profile, so rotating it means editing all of them instead of one env var.
      Caddy's access log records request headers as well as the URI (redacting
      only credential-ish ones like `Authorization`), so the secret lands in the
      log in either form — but for a header that's removable with a log filter,
      whereas the URI is the field the access log exists to record. **Cost of
      the header form:** the three `reqwest` builders
      (`kamaji/src/main.rs:178`, `:223`, `:283`) have no header-injection
      point, so it needs one env var threaded into all three — a one-time
      client change, and the only thing the path form buys is avoiding it.
- [ ] **Zero-code fallback, if that client change is genuinely unwanted:** the
      client appends `/auth/login` etc. to whatever `--remote` base it's given,
      trimming only a trailing slash (`main.rs:179`, `:224`, `:283`), so a
      prefix can ride in the URL the user already passes —
      `kamaji --remote https://host/s3cr3t login` with
      `handle_path /s3cr3t/* { reverse_proxy 127.0.0.1:8081 }` and a sibling
      `respond 404`. `handle_path` strips the prefix, so `kamajid` still sees
      `/auth/login`: `rest.rs` untouched, `KAMAJI_REMOTE_URL` unaffected, and
      the token cache doesn't interact (`read_token()` is not URL-keyed,
      `main.rs:165`). Record it as the cheap-not-better option, with the leak
      surface above as the reason it isn't the default.
- [ ] **Do NOT add a `REST_API_PATH_PREFIX` env var without weighing these
      costs.** It would become a fourth thing that must agree across
      `rest_api_config_from_env` (`kamaji-core/src/config.rs:247-258`), the
      router, the three `format!` call sites in the client, the Caddyfile, and
      `docs/remote-api.md`; a mismatch fails as a bare 404 from axum with no
      hint about which side is wrong. It's also a *shared secret with no
      rotation story*: it lands in `~/.kamaji/` config or a shell profile
      (`KAMAJI_REMOTE_URL`), in shell history, and in Caddy's access log in
      cleartext — unlike the TOTP secret, which never leaves the authenticator
      and the root-only `EnvironmentFile`. If it's added anyway, validate it at
      startup where `expect()` is still allowed per the Rust conventions
      (leading `/`, no whitespace, no `:`/`*` so it can't collide with axum's
      path-parameter syntax) rather than letting a typo silently produce
      unreachable routes.
- [ ] **All three paths or none.** Renaming only `/auth/login` while `/api/cli`
      stays predictable is pure theatre. `/api/cli` is the one that actually
      executes work (`cli_handler`, `rest.rs:158`) — it's bearer-gated, which is
      precisely why the login path's name isn't the weak point either.
- [ ] **Don't sacrifice the deployment smoke test.** `docs/remote-api.md` §4
      verifies Caddy + cert + listener with a `curl` that expects
      `401 {"error":"invalid totp code"}`; a `respond 404` gate or a renamed
      path breaks that check, and a wiring failure then looks identical to a
      working-but-hidden endpoint. Any change here must ship the updated
      one-liner in the same commit, and must keep *some* request that
      distinguishes "reached `kamajid`" from "Caddy/DNS/TLS is broken".
      Relatedly: don't "fix" this by returning 404 instead of 401 for a bad
      TOTP code — it hides nothing (the host is already known) and costs the
      only signal that says the daemon is alive.
- [ ] **The alternative worth more than obscurity, if the itch is real
      exposure:** mTLS at Caddy (client cert per device) or the private overlay
      that `docs/remote-api.md` §Intro already records as deliberately
      declined. Both are actual authentication rather than a secret path. If
      the answer is "not worth the provisioning cost", that's the same answer
      as for the secret path — record it once for both.
- [ ] **Tests:** only if a prefix lands in Rust. `rest.rs`'s test module
      deliberately unit-tests pure logic instead of standing up a full
      `DaemonState` (`rest.rs:229-237`); keep that shape — test the prefix
      *validator/normalizer* as a pure function (accepts `/x`, normalizes
      `x`/`/x/`, rejects empty, whitespace, `:`/`*`), not the router. A
      Caddy-only gate gets no Rust test and shouldn't grow one.
- [ ] **Docs:** whichever way it goes, the outcome belongs in
      `docs/remote-api.md`'s Notes list and `docs/hardening.md` — including the
      "we decided not to" case, so this doesn't get re-proposed as an obvious
      free win later.
