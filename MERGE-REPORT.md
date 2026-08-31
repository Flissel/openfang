# Fork reconciliation: mainline ← deployed credential/subscription line

**Worktree:** `C:/Users/User/ClaudeWork/wt-openfang-fork`
**Branch:** `claude/openfang-fork-reconciliation-v1`
**Merge commit:** `62d561c` (parents: `8a4904b` mainline, `9477056` deployed line)
**Common ancestor:** `39dd2de`
**Status:** merged, verified, committed. Not pushed. No gitlink in any parent repo touched.

---

## 1. Test baselines

All three runs used the identical invocation and environment so the numbers are
comparable: `cargo test --workspace --no-fail-fast -j 2`, with
`CARGO_TARGET_DIR=E:/RustTargets/openfang-fork` and `OPENFANG_HOME` pointed at a
scratch directory inside the worktree (never `~/.openfang`).

| # | Tree | Commit | passed | failed | ignored |
|---|------|--------|-------:|-------:|--------:|
| A | mainline | `8a4904b` | 2827 | 1 | 0 |
| B | deployed line | `9477056` | 2292 | 1 | 0 |
| C | **merged** | `62d561c` | **2861** | **1** | 0 |

The two sides fail *different* tests, and that distinction is the point of
running all three.

**A's failure — pre-existing on mainline, inherited by the merge:**

```
agent::tests::test_vibemind_brain_manifests_use_top_level_mcp_servers
  crates/openfang-types/src/agent.rs:1699
  assertion `left == right` failed:
    agents/brain-researcher/agent.toml has an unexpected mcp_servers scope
     left: ["fetch", "spaces-ideas", "qdrant"]
    right: ["vibemind-db", "fetch", "qdrant"]
```

The manifest and the test guarding it disagree **on mainline**, before any merge.
The deployed line never touches `agents/brain-researcher/agent.toml`, so the merge
neither caused nor fixed it. I left it alone: changing either the manifest or the
expectation is a product decision, not a merge decision.

**B's failure — pre-existing on the deployed line, and *fixed* by the merge:**

```
bundled::tests::parse_researcher_hand
  crates/openfang-hands/src/bundled.rs:169
  assertion `left == right` failed
    left: Some(25)   right: Some(80)
```

This one does **not** survive into C — mainline's newer `openfang-hands`
supersedes it, and the test passes in the merged tree.

**Net effect of the merge:** +34 passing tests over mainline, +569 over the
deployed line, one pre-existing deployed-line failure resolved, and **zero new
failures introduced**.

### A baseline-A artifact I corrected

My first baseline-A run showed a second failure,
`whatsapp_gateway::tests::test_gateway_dir_under_openfang_home`. That was **my
harness, not the code**: the test asserts the gateway directory's parent path
contains `.openfang`, and my scratch `OPENFANG_HOME` was named `ofhome-test`. I
renamed the scratch home to `.openfang` and re-ran. All three baselines above use
the corrected setting.

---

## 2. Clippy

**Required scoped run — passes:**

```
cargo clippy -p openfang-api --lib --tests   ->  exit 0, 0 errors, 11 warnings
```

None of the 11 warnings is in code this merge introduced or touched. They sit in
`openfang-memory/src/runtime_authority.rs` (2),
`openfang-runtime/src/runtime_execution.rs` (1),
`openfang-api/src/routes.rs:7068` (1, far from the credential endpoint at ~13375),
and `openfang-api/tests/api_integration_test.rs:399` (1, mainline's
runtime-admission helper). All pre-existing.

**Correction to the brief.** I was told a workspace-wide clippy fails in
`openfang-memory`. I checked instead of repeating it, and that is not where it
fails:

- `cargo clippy -p openfang-memory --lib --tests` -> **exit 0** (warnings only).
- `cargo clippy --workspace --all-targets` -> **fails in `openfang-runtime`**:

```
error: read amount is not handled
   --> crates/openfang-runtime/src/embedding.rs:364:17
    |  stream.read(&mut request).await.unwrap();
    = note: `#[deny(clippy::unused_io_amount)]` on by default
```

I verified this is pre-existing and unrelated to the merge:
`crates/openfang-runtime/src/embedding.rs` is **byte-identical to mainline
`8a4904b`** in the merge result, and the deployed line never touched that file at
all. So scoping around it is still the right call — just around a different crate
than the brief stated.

---

## 3. Files resolved by hand

Git reported **four** conflicted files. Two more needed hand fixes git could not
see, because mainline widened structs the deployed line constructs. All are below.

### 3.1 `crates/openfang-runtime/src/drivers/claude_code.rs` — kept both sides

Git auto-merged this one without markers, but the brief was explicit that no
tool's output should be accepted here, so I verified it rather than trusting it.

**What each side contributed**

- *Mainline:* `MessageContent` import; `build_prompt` rewritten to call a new
  `render_content`; `render_content` itself (renders non-text blocks as
  `[attachment: ...]` markers instead of dropping them silently); `..Default::default()`
  on the `Message` literal in `test_build_prompt_simple`; two new image tests.
- *Deployed line:* the subscription-wrapper hardening chain — `SUBSCRIPTION_WRAPPERS`,
  `resolve_subscription_wrapper`, `validate_cmd_value` / `validate_cmd_args`,
  `build_cli_command` / `build_cli_command_for_path`, `path_for_cmd`,
  `normalized_extended_path_for_cmd`, the owned `SystemPromptFile`, `build_cli_args`,
  and stream-json `tool_use` observability logging.
- *Both, identically:* one rustfmt reflow in the streaming branch.

**How I verified nothing was lost**

1. **Line arithmetic.** base 905, mainline 1021, deployed 1566, merged **1686** =
   `1021 + 1566 - 905 + 4`, where the `+4` is exactly the shared rustfmt hunk
   (counted in both parents' diffs, applied once).
2. **Diff against each parent.** `merged` vs `deployed` is character for character
   mainline's own ancestor->main diff for this file — all six mainline changes,
   nothing else. I read that diff in full.
3. **Mechanical completeness check.** Zero lines added by the deployed side are
   missing from the result, and **zero lines in the result are absent from both
   parents** (nothing fabricated by the merge).
4. **Behavioural spot-checks.** All eleven hardening symbols present; the argv
   `--system-prompt` fallback is gone (0 occurrences); `write_temp_file` is gone;
   and in *both* `complete()` and `complete_with_tools()` the order is
   `validate_cmd_value(model_flag)` -> `SystemPromptFile::create(..., uses_cmd)` ->
   `validate_cmd_args(cli_args)` -> `cmd.args(...)` -> `cmd.spawn()`. Validation
   still precedes spawn on both paths.
5. All **15** deployed-side hardening tests and both mainline image tests pass.

The five named hardening behaviours, located in the merged file:

| Behaviour | Where |
|---|---|
| unsafe wrapper **arguments** rejected | `validate_cmd_value` rejects control chars and `" ! % & ( ) < > ^ \|`; `validate_cmd_args` applies it to every arg before `cmd.args(...)` |
| unsafe wrapper **aliases** rejected | `path_for_cmd` canonicalises and requires `canonical_candidate == path`, defeating the trailing-dot alias |
| wrapper path **normalised for cmd** | `normalized_extended_path_for_cmd` strips `\\?\UNC\` and `\\?\`, rejecting Volume-GUID and non-ASCII-drive forms |
| **no path fallback** | `build_cli_command` requires `VIBEMIND_SUBSCRIPTION_WRAPPER_DIR`; `resolve_subscription_wrapper` demands an absolute canonical dir and `parent() == that dir` (no symlink escape) |
| subscription agent **runtime contracts** | `crates/openfang-types/tests/subscription_agent_manifest.rs` pins provider, wrapper `base_url`, root `mcp_servers`, and absence of `[mcp_allowed]` |

**One hand fix:** the deployed line's `#[cfg(windows)] subscription_test_request()`
builds a `Message` without `..Default::default()`. Mainline widened `Message` with
`msg_id` / `provider_msg_id`, so that literal no longer compiles. Added
`..Default::default()`.

### 3.2 `agents/brain-video/agent.toml` — the Laura decision

See section 4.

### 3.3-3.5 `AppState` construction in three test files

`crates/openfang-api/tests/{api_integration_test,daemon_lifecycle_test,load_test}.rs`
— five conflicts, all the same shape: mainline had
`budget_config: ...RwLock::new(Default::default())`, the deployed line had
`...RwLock::new(budget)` (or `kernel.config.budget.clone()`) **plus** the new
`issuable_credentials` field.

**Took the deployed side at every site.** It is strictly more faithful (real kernel
budget instead of a default) and it carries the field the merged `AppState` now
requires. I checked the borrow situation before deciding: `daemon_lifecycle_test`
uses `kernel: kernel.clone()` so the later `kernel.config....` is legal, and the
other two get a `let budget = ...` binding that merged in cleanly above the literal.

### 3.6 `api_integration_test.rs` — one conflict where **both** sides added code

Git collided a ~720-line mainline block (the `/api/commands` registry tests and the
clone-agent tests) with a ~420-line deployed block (the credential issuance tests)
at one shared banner comment, and both blocks ended with an unterminated function
whose closing `}` was the shared trailing line.

**Kept both.** I closed mainline's final function, inserted a blank line and a fresh
banner, then appended the deployed block, which the pre-existing final `}` closes.
Verified structurally: brace delta is `2` in the result — identical to base,
mainline, *and* the deployed line (the `2` comes from braces inside string
literals), so no block was left open or double-closed.

---

## 4. The Laura MCP registration — which copy I kept

The same grant is implemented twice. **I kept mainline's and deleted the deployed
line's.**

| Side | Form |
|---|---|
| mainline (kept) | top-level `mcp_servers = ["laura", "vibemind-db"]`, `[mcp_allowed]` section removed |
| deployed (dropped) | `[mcp_allowed]` / `servers = ["laura", "vibemind-db"]` |

This is not a preference — three independent facts force it:

1. **`[mcp_allowed]` is dead config.** `AgentManifest` has exactly one field for
   this, `mcp_servers`, with no `mcp_allowed` field and no serde alias. Nothing
   parses `[mcp_allowed]`; keeping it would have been decoration.
2. **Mainline ships a test that forbids it.**
   `crates/openfang-types/tests/laura_mcp_config.rs` asserts
   `document.get("mcp_allowed").is_none()` and
   `manifest.mcp_servers == ["laura","vibemind-db"]`.
3. **The deployed line agrees with mainline.** Its own new test,
   `crates/openfang-types/tests/subscription_agent_manifest.rs`, asserts
   `!raw.contains("[mcp_allowed]")` — "MCP allowlists must use AgentManifest's root
   `mcp_servers` field". Both lines had independently converged on the same
   contract; only the `brain-video` manifest lagged.

Keeping both would also have left the file failing mainline's own test.

**The two `openfang.vibemind.toml` files were *not* duplicated.** Both sides add a
byte-identical `[[mcp_servers]] name = "laura"` block at the same anchor, and git
recognised it as the same change and applied it once. I verified rather than
assumed:

- `name = "laura"` appears exactly **once** in `openfang.vibemind.toml` and once in
  `openfang.vibemind.toml.template`.
- Both files parse as TOML and contain **no duplicate `mcp_servers` name at all**
  (41 and 38 servers respectively).
- Set comparison against both parents: every server either side added is present,
  the one the deployed line removed (`desktop-automation`, deliberately
  decommissioned) is absent, and nothing appears that neither parent had.
- The deployed line's structural rewrite of the template (the `>>> generated:tools`
  mcp_sync block) survived intact.
- Mainline's `laura_mcp_config` test — which reads both files off disk — passes.

The deployed line's operational settings all survived in `openfang.vibemind.toml`:
`[heartbeat] default_timeout_secs = 360`, `[approval] timeout_secs = 300`,
`telegram default_agent = "brain-gateway"`, and the
`security/pocs/{defense,offense}/...` path corrections.

---

## 5. Merge-induced fixes git could not see

Mainline widened structs that the deployed line constructs. Each was a compile
error the merge itself produced, invisible to a textual merge:

| Struct | Site | Fix |
|---|---|---|
| `AppState` | `skill_config_api_test.rs`, and one `api_integration_test.rs` site | `issuable_credentials: Default::default()` (empty = issuance off, correct for tests that do not exercise it) |
| `Message` | `claude_code.rs` `subscription_test_request()` | `..Default::default()` |
| `DefaultModelConfig` | credential test helper | `subprocess_timeout_secs: None` (the convention at all seven other sites) |
| `AuthState` | credential test helper | `allow_no_auth: true` — see below |

### `allow_no_auth` deserves its own note

This is the one resolution where the obvious-looking strict value was wrong, so I
am spelling out the reasoning.

I first set `allow_no_auth: false` (the strict value, matching production's
env-gated default). That made `test_credential_issue_refused_when_daemon_has_no_auth`
**fail with 401 instead of 404** — the only new failure in the whole merge.

The cause: mainline added fail-closed-for-non-loopback logic to `middleware::auth`.
It passes a request through when `is_loopback || allow_no_auth`. The credential
test router is built **without** `into_make_service_with_connect_info`, so the
middleware cannot observe that the caller is loopback, and with
`allow_no_auth: false` it answered 401 *before* `issue_credential` ever ran. The
handler's own fail-open refusal — the actual security property under test — was
never reached.

`allow_no_auth: true` is therefore what makes the test meaningful, and it is what
mainline's own `start_test_server_with_auth` helper sets, for the same reason.
**It does not weaken anything:** it is test-harness-only, production derives the
flag from `OPENFANG_ALLOW_NO_AUTH` (default `false`) in `server.rs`, and every
credential test server is built with a non-empty `api_key` except the one
deliberately reproducing the fail-open case. With the change, all 8 credential
tests pass — including the two that assert the endpoint refuses without auth.

---

## 6. The credential endpoint was not weakened

The whole `// Credential issuance` region of `routes.rs` is **byte-for-byte
identical** to the deployed line's original (228 lines, verified by extracting and
comparing the region). All three named invariants are intact:

1. **Refuses on a fail-open daemon** —
   `if state.kernel.config.api_key.trim().is_empty() && !state.kernel.config.auth.enabled` -> 404.
2. **The two refusals are indistinguishable** — `resolve_credential` runs *before*
   the allowlist check (so timing matches), and one catch-all `_` arm emits the
   same 404, the same body, and the same single log message for both
   "not allowlisted" and "allowlisted but unresolvable".
3. **Validates before logging** — `is_valid_credential_reference(reference)` returns
   400 at the top of the handler, before any `tracing::` call that interpolates
   `reference`, which is what confines it to `[A-Za-z0-9_]` and stops newline
   injection into a log line.

Wiring also survived: the route is registered in `server.rs` and is **not** in the
middleware's public-path allowlist (and is a POST, which always requires auth); the
`OPENFANG_ISSUABLE_CREDENTIALS` allowlist is read once at startup; and
`rate_limiter.rs` kept **both** sides' entries — the deployed line's
`POST /api/credentials/issue -> 50` alongside mainline's new `skills/reload` and
`skills/*/config` costs.

---

## 7. Live probes

Built with the repo's **`release-fast`** profile (not default release):
`cargo build --profile release-fast --bin openfang` -> finished in **12m34s**,
producing `E:/RustTargets/openfang-fork/release-fast/openfang.exe` (81.8 MB).

Isolated daemon: its own `OPENFANG_HOME` inside the worktree
(`.merge-scratch/probe-home/.openfang`), on **port 47231**, confirmed free
beforehand. No `api_key` configured and `auth.enabled` false — exactly the
fail-open daemon probe 3 needs. **Port 4200 was never touched** (verified below).

### Probe 1 — `/api/health` answers — PASS

```
GET http://127.0.0.1:47231/api/health
{"status":"ok","version":"0.6.9"}
HTTP_STATUS=200
```

### Probe 2 — the subscription driver still refuses an unsafe wrapper argument — PASS

The code path is pinned by tests in `claude_code.rs`; I exercised them against the
merged tree:

```
cargo test -p openfang-runtime --lib drivers::claude_code::tests
  test result: ok. 25 passed; 0 failed

  test_cmd_argument_gate_rejects_shell_and_control_hazards ... ok
  test_cmd_wrapper_rejects_injected_temp_before_spawn_and_allows_safe_spaces_unicode ... ok
  test_cmd_wrapper_path_rejects_metacharacters_before_command_build ... ok
  cmd_gate_behavior_child ... ok   (x3: temp / model / safe modes)
```

The second of these is the end-to-end one: it writes a real wrapper `.cmd`, then in
`model` mode sets `request.model = "claude-code/sonnet&echo injected"` and asserts
the driver **errors and the wrapper is never spawned** (a marker file the wrapper
would create must not exist), while a safe invocation still returns `SAFE_OK`. That
is precisely "an unsafe wrapper argument is refused before spawn".

### Probe 3 — `POST /api/credentials/issue` refuses on a daemon with no API key — PASS

An obviously fake reference was used; no real secret was involved at any point.

```
POST http://127.0.0.1:47231/api/credentials/issue
     {"reference":"FAKE_PROBE_TOKEN_NOT_REAL"}

HTTP/1.1 404 Not Found
content-type: application/json
cache-control: no-store, no-cache, must-revalidate
{"error":"credential_unavailable"}
```

A 404 alone would be ambiguous (an unregistered route also 404s), so I confirmed in
the daemon log that the request actually reached the handler and took the fail-open
branch:

```
INFO  openfang_api::server: Credential issuance disabled (OPENFANG_ISSUABLE_CREDENTIALS unset or empty)
WARN  openfang_api::routes: Credential issuance refused: this daemon runs without
      authentication (api_key is empty and auth.enabled is false), which disables
      the endpoint. Set api_key, or enable auth, to use it. reference=FAKE_PROBE_TOKEN_NOT_REAL
INFO  openfang_api::middleware: method=POST path=/api/credentials/issue status=404
```

### Bonus probe — log-injection guard, live — PASS

```
POST {"reference":"FAKE_TOKEN\nWARN openfang: injected fake log line"}
  -> 400 {"error":"reference_invalid"}
grep 'injected fake log line' daemon.log  -> NOT LOGGED
```

Validation ran before logging; the newline never reached a log line.

### Teardown

Probe daemon stopped, port 47231 free again, probe home deleted. Port 4200 verified
untouched **before and after**: same process (PID 38700,
`vibemind-os/openfang/target/release/openfang.exe`, started 11:09:23) still
listening. That is a different binary from the one I built, and I never stopped,
restarted, replaced or wrote to it, nor to `~/.openfang/`.

---

## 8. Things I did not change, and flag for you

1. **The pre-existing `brain-researcher` test failure** (section 1). Fixing it means
   editing either the manifest or the expectation — a product decision, not a merge
   decision.
2. **Four committed `.pyc` files** come in from the deployed line
   (`mcp/__pycache__/*.cpython-311.pyc`). Neither side's `.gitignore` covers
   `__pycache__`, so they were committed deliberately or by accident upstream. I
   kept them — deleting them is not a merge decision — but they are build artifacts
   and probably want removing plus a `__pycache__/` ignore rule.
3. **`openfang.vibemind.toml.template` drift.** The deployed line's `[heartbeat]`,
   `[approval]` and `telegram default_agent = "brain-gateway"` changes were made
   only in the concrete `openfang.vibemind.toml`, never in the `.template`. The merge
   faithfully reproduces that asymmetry. If the template is meant to track the live
   config, it is currently behind — but that drift predates this merge.
4. **Two schema files stay uncommitted.**
   `crates/openfang-desktop/gen/schemas/{desktop,windows}-schema.json` show as
   modified in the worktree but have an empty content diff (CRLF renormalisation
   only). They were dirty before I started and are not mine, so I left them out of
   the merge commit.

---

## 9. Reproduction

```powershell
$env:CARGO_TARGET_DIR = "E:/RustTargets/openfang-fork"
$env:OPENFANG_HOME    = "<worktree>/.merge-scratch/.openfang"   # never ~/.openfang

cargo test --workspace --no-fail-fast -j 2
cargo clippy -p openfang-api --lib --tests
cargo build --profile release-fast --bin openfang
```

`-j 2` is not cosmetic on this machine. The default job count ran six concurrent
`link.exe` processes holding ~3.3 GB each, which exhausted 32 GB of RAM and drove
the box into pagefile thrash on `E:` — which is a spinning HDD, not the SSD. The
linkers made ~0.08 CPU-seconds of progress per 45 seconds of wall clock. It also
left a corrupt PDB behind (`LNK1207`) that had to be cleared before the build would
link at all.

Logs from every run are under `.merge-scratch/logs/` (untracked).
