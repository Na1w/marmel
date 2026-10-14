# Recon Bug Audit — harness (fs/search/sandbox/pty/workspace/sleep/plan), llm, net, ui bridge/transcript/raw, config, types

Method: read-only inspection of all in-scope files; experiments: Python re-implementations of the
`phase.rs` regexes (findings C1-C3) and inspection of the vendored `portable-pty 0.9.0` source
(confirms no child reaping; `pre_exec` `setsid` ⇒ pid == pgid). No source file was modified.
Rehydration / crash-journal / deep-freeze code excluded. Ordered by severity within each area.

---

## 1. Harness — filesystem, search, sandbox

**H1 — `src/harness/mod.rs:113-121` + `src/types/tools.rs:443-452` + `src/mcp/mod.rs:43-47` — High — MCP tools dispatched first can shadow built-ins**
`dispatch()` checks `mcp.has_tool(&tool.name)` before the built-in match, and MCP names are
`format!("{}__{}", server_name, name)` with no validation of `server_name`. A server named
`terminal` exposing `run_command` produces `terminal__run_command`, which is a built-in alias
(`tool_names.rs:55`, matched at `harness/mod.rs:132`) ⇒ commands silently bypass the sandboxed PTY
runner (no Landlock, no timeout clamp). Fix: reject server names `terminal`/containing `__`; move the
MCP lookup after the built-in match.

**H2 — `src/harness/fs.rs:105-135` — Medium — `read_file` has no size cap** — `std::fs::read` (112) reads the whole file, `from_utf8_lossy` (113) copies it, then `chars().count()` + skip/take/collect materialise further copies: `read_file` on a multi-GB log or `target/` artifact allocates 2-3× the file ⇒ OOM kill of the TUI. Fix: size guard (≤1-2 MB) before reading; stream and early-exit instead of collecting all lines.

**H3 — `src/harness/fs.rs:206-…` (`str_arg`) — Medium — write path inferred from tool content** — when `path` is absent the fallback scans other arguments (including `content`) for a `saved to \`...\`` hint, so `{"content": "code saved to \`src/main.rs\`…"}` intended for a new file silently overwrites `src/main.rs`. Fix: require an explicit `path`, error otherwise.

**H4 — `src/harness/fs.rs:161-170` — Medium — temp-file collision and leakage in `replace()`** — the temp name `.{file}.tmp.{pid}` is identical for two `replace()` calls on the same file within one process (parallel specialists, or two replaces in one batch), so the second write clobbers the first staged content before rename ⇒ one edit lost with no error; a failed `rename` also leaves the temp file in the workspace. Fix: random/counter suffix + `let _ = remove_file(&tmp)` on failure.

**H5 — `src/harness/fs.rs:37-…` (`resolve_safe_path`) — Medium / UNCONFIRMED — fail-open on canonicalize error**
For paths whose parent is missing it does `cur.canonicalize().unwrap_or(cur)` and then a purely
textual `starts_with(root)`; an unresolved component containing `..` (canonicalize failing with
EACCES on an intermediate dir, or ELOOP) can pass the check. Needed evidence: concrete errno repro.
Fix: reject when canonicalize fails on an existing path; normalise `..` lexically before the test.

**H6 — `src/harness/fs.rs:23` (`map_path`), `:37` — Low — string prefix instead of component match**
`starts_with("/home/coder/workspace")` also matches `/home/coder/workspace.bak/x` ⇒ maps into a
sibling tree. `map_path` is dead (only `fs_tests.rs`), but the same logic is live in
`resolve_safe_path`. Fix: component-wise containment check.

**H7 — `src/harness/fs.rs:206-…` (alias list) — Low — `name`/`doc`/`target` treated as paths** — `{"name": "x"}` becomes a file path, compounding H3. Fix: drop non-path aliases.

**H8 — `src/harness/search.rs:36-58` — Medium — `grep_search` truncates without notice**
The loop returns as soon as `results.len() >= max_results` with no footer (unlike `glob`). 900
matches for `grep_search("TODO")` ⇒ model treats 100 lines as the complete set and reports "all
fixed" after a partial refactor. Fix: count total (bounded) and append `[showing 100 of N]`; expose
`max_results` in the schema.

**H9 — `src/harness/search.rs:44-46` — Medium — binary/oversized files silently skipped; no size cap**
`if let Ok(content) = std::fs::read_to_string(path)` swallows both read errors and non-UTF-8
content; there is no `max_filesize` and no ignore-crate size limit. A 1.5 GB binary in a
non-gitignored dir (`target/`, `node_modules/`, vendored data) is read into a String ⇒ hang/OOM;
genuine matches in binary-ish files vanish silently. Fix: `WalkBuilder::max_filesize(...)`, count
and report `skipped N binary/oversized files`.

**H10 — `src/harness/search.rs:104-130` (`glob_to_regex`) + `glob_in_root` — Medium — unsupported glob syntax fails silently; unbounded collection**
`{a,b}` and `[...]` are escaped literally, so `**/*.{rs,toml}` matches nothing and the tool answers
"no matches" for a valid pattern. `glob_in_root` also collects **all** matches, then sorts and
`truncate(500)` ⇒ unbounded memory on `**/*`, and no truncation notice. Fix: use `glob`/`globset`
or implement braces/classes; stop at `limit+1`; add footer.

**H11 — `src/harness/search.rs:44` — Low — absolute host paths in results** — `grep_search` returns canonical `/home/<user>/…` paths while `glob` returns root-relative ones; leaks host usernames into transcripts and feeds absolute paths back into `read_file`. Fix: root-relative everywhere.

Verified non-defect: `require_git(false)` is set, so `.gitignore` is honoured outside git repos (confirmed by the existing `search.rs` test); `sandbox.rs` and `common.rs` showed no logic defect.

---

## 2. Harness — PTY, processes, sandbox, workspace

**P1 — `src/harness/pty.rs:182`, `:313`, `run_command_pty` (`:223`) — High — zombie child processes**
Only `kill()` is called; there is **no** `wait()`/`try_wait()` anywhere in `pty.rs` (grep-verified),
and vendored `portable-pty 0.9.0` returns a plain `std::process::Child` with no reaping Drop impl.
A 200-tool-call session leaves ~200 `<defunct>` entries; under a pid/`ulimit -u` limit the next
spawn fails with EAGAIN and every later `run_command` errors. Fix: poll `try_wait()` after the read
loop / `wait()` with a short grace timer after `kill()`.

**P2 — `src/harness/pty.rs:80-…` (`build_sandboxed_command`) — High — sandbox silently disabled for renamed binaries**
The Landlock re-entry (`--internal-sandbox-exec`) is gated on the executable being named exactly
`marmel`. Running as `marmel-debug`, via `cargo run`, or a differently packaged binary ⇒ every
`run_command` executes **unsandboxed**, with no warning in UI or log. Fix: decide from config /
`/proc/self/exe`; log a loud warning when sandboxing is skipped.

**P3 — `src/harness/pty.rs:286-288`, `:407`, `:462-464`, `:512-514`, `:552-554` — Medium/High — unbounded interactive buffer**
`SharedBuffer { output: Vec<u8>, cursor }` only appends; readers advance `cursor` but never drain.
A `pty_spawn`ed watch loop / `yes` / `tail -f` grows the Vec without bound (hundreds of MB/hour)
even with regular `pty_read` calls ⇒ OOM. Fix: `output.drain(..cursor); cursor = 0;` plus a
retention cap with an explicit `[N bytes dropped]` marker.

**P4 — `src/harness/pty.rs:~286-305` — Medium — partial output discarded on timeout/cancel**
The timeout path returns only `[command timed out after Ns and was killed]`; bytes already captured
by the reader thread are dropped (same for cancellation/abrupt exit). `cargo build` exceeding the
300 s clamp gives the model zero compiler output, so it re-runs the identical command. Fix: return
`partial_output + "\n[command timed out…]"`.

**P5 — `src/harness/pty.rs:223-285` — Medium — reader thread never joined**
The reader thread is spawned and abandoned; if `kill()` fails or the pty master stays open, it
blocks in `read` forever ⇒ steady thread growth across a long session. Fix: keep the `JoinHandle`,
close the master / set a shutdown flag, `join()` with timeout.

**P6 — `src/harness/pty.rs:330-…` (`PtyManager::new` reaper) and `pty_spawn` — Medium — idle reaper kills live sessions; failed spawn destroys the previous one**
Sessions idle > 300 s are reaped purely on output silence: an interactive `gdb` session or a long
silent build is destroyed mid-work ("session not found" on next `pty_read`). `pty_spawn` also calls
`self.close(&key)` **before** `openpty`, so a spawn error wipes the existing session with that id.
Fix: reap only when the child also exited; close the old session only after a successful spawn.

**P7 — `src/config.rs:112`/`145`/`251`/`327` + `src/harness/pty.rs:206-221` — Medium — `command_timeout_secs` parsed/merged but never read**
`run_command` derives its timeout from the tool argument with `clamp(1, 300)` and never consults the
config knob. Operator sets `command_timeout_secs = 30` for governance; commands still run up to 300
s. Fix: use it as default and ceiling.

**P8 — `src/harness/pty.rs` (`pty_read`/`pty_write`) — Low/Medium — no clamp on `wait_ms`** — `pty_read {"wait_ms": 3600000}` (raw `u64` from the model) blocks the whole turn for an hour, unlike the 1-300 s clamp in `run_command`. Fix: `wait_ms.clamp(0, 10_000)`.

**P9 — `src/harness/pty.rs` (`sanitize`, RE_OSC) — Low** — `\x1b\][0-9]+;.*?(?:\x07|\x1b\\)`: `.` does not match `\n`, so OSC sequences spanning lines reach the UI raw. Fix: `(?s)` or `[^\x07\x1b]*`.

**W1 — `src/ui/helpers.rs:415-425` (`load_prompt_from_disk`, live) + `src/harness/workspace.rs:~88` (`prompt_path_for_task`, currently dead) — Medium — task id used raw in a path join**
`clean_task_id` (`helpers.rs:369`) only trims `[]()"'`; `/` and `..` survive and are joined as
`p.dir().join("prompts").join(format!("{clean}.md"))`. A plan or `delegate_task` argument with task
id `../../../../etc/…` reads an arbitrary `.md`-suffixed file outside the workspace and injects it
into a specialist system prompt (info disclosure + prompt injection). Fix: whitelist ids with
`^[A-Za-z0-9._-]+$` before joining.

**W2 — `src/main.rs:85` — Low / UNCONFIRMED — 300 ms runtime shutdown timeout** — `rt.shutdown_timeout(300ms)` after `cancel_all()` can truncate in-flight PTY teardown/reap, leaving orphaned shells. Needs repro with an interactive session at exit. Fix: close `PtyManager` sessions before dropping the runtime.

Verified non-defect: `workspace.rs::rotate_log_file` is correct for `backups >= 1` (covered by `main.rs` tests). `sleep.rs` clamps 1..300 s correctly, but `handle_sleep` (sync) and `handle_sleep_async` are two divergent copies with a duplicated hardcoded `max_sleep = 300` (Low, maintainability).

---

## 3. Plan parsing / check-off (`src/manager/phase.rs`, reached via `src/harness/plan.rs`)

**C1 — `phase.rs:64` (`TASK_LINE_RE`), `:476` (`check_off`) — High — parenthesised task ids never match**
Both regexes accept only `[t-001]` (plus optional bold markers), not `(t-001)`; verified by
re-running the patterns in Python. Plan line `- [ ] (t-002) migrate schema` ⇒ `pending_tasks()`
returns `[]` (so the pending-tasks notice at `session.rs:507` lists nothing) **and**
`check_off("t-002")` fails with "task id not found or already checked" — which `session.rs:1100` and
`:1486` discard with `let _ =` and `manager/loop.rs:524` only `warn`s. Result: the plan can never
reach `is_complete()`, `archive()` refuses, and the loop re-delegates the same work indefinitely.
Fix: one shared regex accepting `\[id\]` and `(id)`; propagate `check_off` errors to the UI.

**C2 — `phase.rs:64`/`:74` vs `:476` — Medium — list-marker grammars disagree**
`check_off` accepts `\d+\.` but `TASK_LINE_RE`/`ALL_TASKS_RE` require `[-*]`. Verified:
`1. [ ] [t-004] x` is checkable yet invisible to `pending_tasks()`/`all_tasks()` ⇒ progress
summaries report 0 tasks and completion fires early while check-off "works". Fix: single shared
regex constant.

**C3 — `phase.rs:74` (`UNCHECKED_BOX_RE = \[\s*\]|\(\s*\)`) — Medium — global empty-bracket scan blocks completion**
Not anchored to task lines. Verified: `- [x] [t-001] add () guard` (or prose containing `[]`) makes
`is_complete()` permanently false ⇒ `archive()` refuses with "plan is not complete" forever.
Fix: anchor, e.g. `^[\s>*-]*\[\s*\]`.

**C4 — `src/harness/plan.rs` (`write_plan_internal`) — Medium — validation failures fail open**
On validation error the code logs `tracing::warn!("Plan validation skipped")` and writes the
**unvalidated** plan, so malformed plans (C1-C3) reach disk and then stall the loop. Fix: return
`ToolResult::err` so the model can re-emit a valid plan.

**C5 — `phase.rs:83` (`output_is_success`) — Low (latent) — substring success heuristic** — substring tests for `ERROR`/`FAILED` mark successful output mentioning `src/error.rs`/`error_handling` as failure, and the sanitization list is order-dependent (verified: `TESTS FAILED: 0` still fails). Today only re-exported (`manager/mod.rs:12`) and called with literal "ok" (`manager/loop.rs:524`). Fix: key off exit-code markers, not prose.

**C6 — `src/harness/plan.rs:~106` (`archive_plan`) — Low** — uses `Plan::default()`, ignoring custom plan dir / `with_workspace_root` scoping, so archiving looks at `.marmel/execution_plan.md` for sessions using another root. Fix: thread the configured `Plan`.

**C7 — `src/orchestrator/plan_summary.rs:37-62` — Low** — `trim_start_matches(|c| c.is_ascii_digit() || c == '.')` corrupts task text: `"3b: migrate schema"` → `"b: migrate schema"`. Fix: strip only a leading numbered-list marker via regex.

---

## 4. LLM — client, stream, thinking

**L1 — `src/llm/client.rs:~470` (`consume_event`) — High — cancellation/truncation reported as success**
`if !on_delta(&c) { return Ok(true); }` and `PumpNext::Aborted → Ok(StreamedReply::default())`.
Pause/steer or a budget cut after half a `write_file` argument list yields an *empty successful*
reply indistinguishable from "model produced nothing": the caller re-nudges (doubled cost) or
executes a truncated tool call. Fix: return a `Cancelled`/`Truncated` variant carrying partial text
and the reason (Cancelled / Deadline / Idle).

**L2 — `src/llm/client.rs:~110` (`is_retryable`) — High/Medium — full retry after partial streaming**
`Stream`/`Transport`/`StallTimeout`/`ReadTimeout` are retried even after thousands of deltas already
passed through `on_delta` (transcript + renderer). Connection drop at token 4 000 ⇒ the request is
replayed from scratch: doubled token cost and the first 4 000 tokens rendered twice. Fix: retry only
zero-delta attempts; otherwise surface a resumable error (`chat_stream_resumable`).

**L3 — `src/llm/stream.rs:~265` (`on_chunk`) — Medium — `max_stream_tokens` counts SSE deltas, not tokens**
`tokens_count += 1` per chunk vs `max_stream_tokens` (32768 default). Word-sized deltas (~5 chars per
delta) hit the cap after ~65 K chars, i.e. cut off at ~1/5 of the intended budget mid-sentence;
paragraph-sized deltas never hit it at all. Fix: accumulate `chars()/4` (or bytes/4).

**L4 — `src/llm/stream.rs:~560` (`chat_stream_resumable` error path) — Medium — partial tool-call fragments resurrected**
The fallback continuation reuses the already-accumulated handler and `all_tool_calls.extend()` keeps
**partial** fragments from the paused/aborted stream; `wire.rs::ensure_valid_json_arguments` wraps
the malformed args as `{"_error":"malformed_arguments"}` and the tool is then **executed** with
garbage arguments. Fix: drop tool calls whose `arguments` were not JSON-complete at abort time.

**L5 — `src/llm/thinking.rs` (`apply_recovery`) — Medium — unclamped parameter escalation**
`temperature += 0.1`, `frequency_penalty += 0.5` with no clamp; with the base config at
`temperature = 2.0`/`frequency_penalty = 2.0` the recovery request sends 2.1/2.5 ⇒ HTTP 400
(non-retryable) — a hard failure precisely when recovery was supposed to rescue the turn.
Fix: `temperature.min(2.0)`, `frequency_penalty.clamp(-2.0, 2.0)`.

**L6 — `src/types/wire.rs:~455` (`ChunkChoice.delta`) — Medium — usage-only chunks silently dropped**
`delta` is non-optional without `#[serde(default)]`, so chunks lacking it (OpenAI-compatible usage
chunks, some vLLM/Ollama builds) fail `from_str`, swallowed by `if let Ok(chunk)` at
`client.rs:~480`. Consequences: token/usage accounting lost and `finish_reason`-only chunks ignored;
stream-end detection depends solely on `data: [DONE]`. Fix: `Option<ChunkDelta>` + `default`, log
parse failures.

**L7 — `src/types/wire.rs:~428` (`ChatRequest`) and `src/llm/stream.rs:~200` (`build_request`) — Medium — no `max_tokens`/`max_completion_tokens`**
The output budget is enforced client-side only, so a runaway completion is billed in full. Fix: add
and populate per backend capability.

**L8 — `src/llm/client.rs:~150` (`record_tokens_in`) — Medium — retries multiply accounting**
Prompt tokens are recorded per attempt ⇒ a retry doubles reported session cost; also
`count_reply_tokens` + `record_tokens_out` is duplicated across three return paths (drift risk).
Fix: record once per logical turn via a single helper.

**L9 — `src/llm/client.rs:~185` — Low/Medium — first-event watchdog measured from request start** — `send_deadline = first_start + initial_timeout_secs` is reused as the first-event deadline, so a slow upload of a large prompt consumes the entire first-token budget ⇒ spurious "no first event" timeout. Fix: separate `first_event_deadline` started after the response head arrives.

**L10 — `src/llm/client.rs:~215` — Low** — `resp.text().await.unwrap_or_default()` and `serde_json::to_string(&req_body).unwrap_or_default()` turn real failures into empty-body/"empty response" diagnostics. Fix: `unwrap_or_else` with `<unreadable body: {e}>`.

**L11 — `src/llm/stream.rs:~290` (`drive_streamed_turn`) — Low (test-only path)** — `reply.raw` concatenates reasoning + content with no delimiters and is re-demuxed, so backend `reasoning_content` reappears as visible assistant text (double render). Only `llm/stream_tests.rs` reaches it. Fix: delete the re-demux or carry explicit tags.

Verified safe: `thinking.rs::push_delta` scanning is cross-pair tolerant and
`partial_prefix_split` retains at most `tag_len-1` bytes, slicing only on `find` indices ⇒ no
char-boundary panic, no unbounded pending buffer.

---

## 5. Net — http / sse / retry

**N1 — `src/net/sse.rs:38-78` (`pump_next`) — Low/Medium — `abort("")` doubles as the sink-callback probe** — the same empty-reason abort path covers deadline expiry and a sink callback returning `false`, so a sink that rejects an empty/heartbeat delta (e.g. role-only delta) silently aborts a healthy stream, indistinguishable from a timeout. Fix: distinct `PumpNext::SinkRejected` variant / non-empty reason.

**N2 — `src/net/retry.rs:20-24` — Medium/Low — linear backoff, no jitter, `Retry-After` ignored** — delays are 1 s, 2 s and 429/503 `Retry-After` is not honoured; concurrent specialist clients hammer a rate-limited endpoint in lockstep and exhaust the retry budget instead of waiting the advertised 30 s. Fix: full jitter + honour `Retry-After`.

**N3 — `src/net/sse.rs` — Low** — `let mut next = stream.next();` re-created on every 50 ms tick is safe (stream state preserved) but allocates and re-polls each tick, wasting wakeups under load. Fix: `tokio::time::timeout` around `stream.next()`.

---

## 6. Config & debug logging

**G1 — `src/config.rs:~95` and `~215` — Medium — unknown keys silently ignored**
No `deny_unknown_fields` on `Config`/`PartialConfig`/`SpecialistConfig`: `max_stream_token = 8192`
or `tools = [...]` (instead of `tool_names`) is dropped silently and the session runs with defaults
(e.g. an unbounded stream budget). Fix: `deny_unknown_fields`, or warn on unknown keys.

**G2 — `src/config.rs:~180` (`resolve_config_path`) — Medium — documented layering not implemented**
Returns the **first** existing candidate, so `./.marmel.toml` completely shadows
`~/.config/marmel/config.toml`. A project file containing only `model` makes the user's global
`auth_token`/`backend_url`/`command_timeout_secs` disappear ⇒ "unauthorized"/default timeouts.
Fix: merge global → project → CLI layers.

**G3 — `src/config.rs:~160-175` — Low/Medium — inconsistent env precedence** — `MARMEL_BACKEND_URL`/`MARMEL_MODEL` always override the file, but `MARMEL_AUTH_TOKEN` applies only `if cfg.auth_token.is_empty()` ⇒ a CI-exported token loses to a stale file token (401s, or requests against the wrong account). Fix: env always wins.

**G4 — `src/config.rs:118` vs `monitoring.max_thinking_tokens` — Medium — two knobs, one setting**
Top-level `max_thinking_tokens` is used at `agents/runner/execution.rs:189` (specialist turns) while
`llm/stream.rs:167` (`StreamConfig::from_config`) reads `monitoring.max_thinking_tokens`;
`marmel.toml.example` documents both ⇒ specialists and manager/UI turns run with different thinking
budgets and the cost behaviour looks inconsistent. Fix: one knob, deprecate the other with a warning.

**G5 — `src/debug_log.rs:22-45` — Medium — `debug.log` never rotated; second `init()` silently ignored**
`rotate_log_file` is applied only to `marmel.log` (`main.rs:437`), so with `--debug` every attempt
appends the full transcript payload to an ever-growing file. The `OnceLock` init also keeps the
**first** file while still setting `DEBUG_LOG_ENABLED = true` ⇒ later writes land in an unexpected
path. Fix: rotate on open; warn on re-init.

Verified safe: `expand_paths`/`home_dir` (`config.rs:~380`) — `Path::strip_prefix("~")` yields the
remainder, not `"/…"`.

---

## 7. UI — bridge, transcript, raw, session, helpers

**U1 — `src/ui/bridge/steer.rs:151-184` + `src/ui/bridge/drain.rs:179-188` — High — steering instruction lost**
`spawn_steer_arbitration` loops `while loop_count < 5` on `Sleep`; when it exits with decision
`Sleep` (or `break`s at `:179`/`:184` after a cancelled sleep), the terminal `Finished` event is
handled by the `"Sleep" =>` arm in `drain.rs:179-184`, which only emits an `Event::Status` — no
`steer_queue.push(user_msg)` (cf. `:188`, `:209`) and no `UiRecord` append. Five consecutive
"sleep" verdicts ⇒ the user's instruction is never queued for the next turn and never recorded; it
survives only as a transient status line. Fix: on terminal Sleep, queue the message as a pending
steer and append a transcript record.

**U2 — `src/ui/bridge/sink.rs:81-135` + `:328` — High/Medium — up to six consecutive sleeps (≤ 30 min)**
The inner `while loop_count < 5` sleep loop (each sleep up to 300 s) can fall through to the outer
`"Sleep" =>` arm at `sink.rs:328`, which sleeps **again**. A user pausing to steer can leave the
stream paused and sleeping for 30 minutes with no signal that sleeping continues. Fix: return the
inner-loop decision instead of re-sleeping.

**U3 — `src/ui/bridge/drain.rs` vs `src/ui/bridge/sink.rs` — Medium / UNCONFIRMED — duplicated decision switch ⇒ possible double steer enqueue**
Both files implement the full decision-handling switch (queue pushes, cancels, aborts). If one user
message flows through both the background arbitration path (`arb_tx`) and `on_pause`, it can be
pushed into `steer_queue` twice ⇒ the orchestrator executes the same instruction twice. Needed
evidence: a transcript with two `UiRecord::User` entries / two pushes for one message. Fix: extract
one `apply_decision()` used by both callers.

**U4 — `src/ui/bridge/steer.rs:157-161` + `:290-293` — Low/Medium — duplicated steering-history entries** — each Sleep iteration pushes `(msg, "{note} (slept for {n}s)")` and the final push adds the same `msg` again, so one instruction appears up to 6 times in `format_steering_history`, biasing the arbitrator prompt and inflating context. Fix: one entry per message id, updated in place.

**U5 — `src/ui/session.rs:47` — Low/Medium — unbounded arbitration channel** — `mpsc::unbounded_channel::<SteerArbEvent>()`; while the stream is not polling the bridge (long tool execution) arbitration events accumulate without bound. Fix: bounded channel (e.g. 256) + backpressure warning.

**U6 — `src/ui/bridge/sink.rs:33-45` + per-event `renderer.set_subagents(sub.clone())` in `drain.rs` — Low** — `emit()` flushes on every delta and `reset_active_agent()` before every Message/Thinking event, and drain clones the whole subagent list per event ⇒ stutter and CPU saturation at thousands of deltas/s. Fix: flush on a ~16 ms timer or block boundaries.

**U7 — `src/ui/raw.rs:~63` (`push_line`) — Medium — embedded newlines break one-line-per-record**
`writeln!(buffer, "[{label}] {chunk}")`; `chunk_utf8` bounds length but never escapes newlines, so
continuation lines carry no `[label]` prefix despite the documented "one logical unit per line"
contract. `marmel --raw | grep '^\[delta\]'` loses multi-line model output and scripted parsing
breaks. Fix: escape `\n` (or repeat the label on every physical line).

**U8 — `src/ui/raw.rs:~110` (`on_event`) — Low** — every delta locks, writes and flushes stdout. Fix: buffered writes with time-based flush.

**U9 — `src/ui/transcript.rs:103` (`load`) — Low — hard fail on any corrupt record** — a truncated final JSON line (crash mid-append) makes the whole load return `Err`, and callers drop the error ⇒ the entire journal is treated as empty. Fix: parse line-by-line, keep the good prefix, report the corruption.

**U10 — `src/ui/transcript.rs:104-148` (`extract_steering_history`) — Low** — a new `User` record overwrites `current_user` before any `SteerResponse` (unanswered inquiry silently dropped), and the "pending decision" heuristic matches literal substrings including Swedish `följdfråga`, `awaiting specialist reply`, `Decision:` ⇒ breaks on any reworded prompt. Fix: typed pending-decision records.

**U11 — `src/ui/transcript.rs:~155` (`from_legacy_messages`) — Low** — `skip(1)` assumes index 0 is the system prompt; if absent, the first real user message is dropped. Fix: skip only when `messages[0].role == "system"`.

**U12 — `src/ui/session.rs:1100`, `:1486` — Medium (amplifier of C1) — `let _ = plan.check_off(tid)`**
Check-off failures are discarded in the live UI path, so C1/C2 are invisible: the plan widget never
updates and the model is never told the check-off failed. Fix: surface as `Event::Status`/warning.

**U13 — `src/ui/helpers.rs:178-191` (`is_abort_command`) — Low** — matches bare `abort`, `exit`, `quit`, `stop`, `cancel`, `q`, `:q`, so a one-word steering message such as `exit` aborts/exits from `poll_control` (`sink.rs:~55`). Fix: require the leading `/` and document aliases.

**U14 — `src/ui/helpers.rs:~409`, `:435` — Low** — a `cfg.system_prompt_path` that exists but fails to read falls back to the embedded prompt with no warning (operator believes their prompt is active); `load_prompt_from_disk` ends in `read_to_string(&prompt_file).unwrap_or_default()`. Fix: `tracing::warn!` + status-line hint.

**U15 — `src/ui/helpers.rs:~380` (`format_active_subtasks`) — Low** — elapsed time uses `started_at` rather than accumulated `worked_duration` (and the `global_active` override returns early), so paused-then-resumed subagents show wall-clock instead of work time. Fix: prefer `worked_duration` when non-zero.

Verified safe: `helpers.rs:157 chunk_utf8` splits on char boundaries; `find_subagent_mut`
(`helpers.rs:376-405`) matching is loose (`ends_with`) but bounded; `manager_tools` correctly omits
`write_file`/`replace`/`run_command` (by design).

---

## 8. Cross-cutting — main, tools, dispatch

**X1 — `src/main.rs:~113` (`parse_args`) — Low/Medium — unknown flags become the prompt** — unrecognised tokens fall into the `prompt` bucket: `marmel --debu refactor auth` sends the literal `--debu refactor auth` as the session goal with no typo warning (whole session wasted). Fix: error on unknown `--` flags.

**X2 — `src/types/tools.rs:412` (`minify_json_schema`) — Low** — recursively strips `$schema`, `$id` **and** `title`; for MCP schemas where `title` is the only discriminator across `oneOf` branches the model loses that information (returning `{"type":"object"}` for empty schemas is fine). Fix: strip only at the root.

**X3 — `src/main.rs:~25` (`--internal-sandbox-exec`) — Low (hygiene)** — the argv-derived command runs with no allow-list and cwd is unvalidated (missing dir ⇒ exit 1, acceptable); only self-invocation reaches this path. Fix: assert the argv sentinel and re-derive cwd canonically.

**X4 — `src/harness/mod.rs:110-121` (`block_on_safe`) — Low** — `panic!("failed to initialize tokio runtime for synchronous bridge")` inside tool dispatch aborts the process rather than returning `ToolResult::err`. Fix: return `ToolError`.

---

## UNCONFIRMED / needs evidence
- H5 (`resolve_safe_path` fail-open): needs a concrete errno repro (EACCES/ELOOP on an existing path).
- U3 (double steer enqueue through both arbitration paths): needs a transcript showing one message
  pushed twice / two `UiRecord::User` entries.
- W2 (orphaned shells at exit): needs a repro with an interactive PTY session at shutdown.

## Short fix list (highest value first)
1. `phase.rs:64/74/476`: single shared task-line regex accepting `(id)` and `1.` forms; anchor `UNCHECKED_BOX_RE`; stop discarding `check_off` errors (`session.rs:1100/1486`, `harness/plan.rs` validation must fail closed).
2. `pty.rs`: `try_wait()`/`wait()` after every `kill()` (zombies); join reader threads; drain `SharedBuffer.output` into a ring buffer with a drop marker.
3. `pty.rs:80`: never gate Landlock on the binary being named `marmel`; warn loudly when sandboxing is skipped.
4. `llm/client.rs:~470/~110`: return distinct `Cancelled`/`Truncated` results; forbid retry once deltas were emitted (use the resumable path instead).
5. `steer.rs`/`drain.rs`: never drop a terminal-Sleep steer (queue + record); fix the sleep fall-through `sink.rs:85→328`; de-duplicate `steering_history`; share one `apply_decision()`.
6. `search.rs`: truncation footers for `grep_search`/`glob`; `max_filesize` + skipped-file counts; implement (or delegate) glob braces/classes.
7. `fs.rs`: size cap in `read_file`; unique temp names + cleanup in `replace()`; require explicit `path` (drop content/`name` inference).
8. `harness/mod.rs` + `mcp`: validate MCP server names, resolve built-ins before MCP.
9. `helpers.rs:415-425` / `workspace.rs:~88`: whitelist task-id characters before path joins (traversal + prompt injection).
10. `config.rs`: `deny_unknown_fields`, real config merging, env-always-wins for `auth_token`, unify the two `max_thinking_tokens` knobs, actually apply `command_timeout_secs`.
11. `stream.rs:265`: count real tokens (chars/4) against `max_stream_tokens`; add `max_tokens` to `ChatRequest`; `#[serde(default)] Option<…>` for `ChunkChoice.delta`; drop incomplete tool-call args on resume.
12. `raw.rs:63`: escape/label newlines; `debug_log.rs`: rotate `debug.log` + warn on re-init; `main.rs`: reject unknown flags; `thinking.rs`: clamp recovery params; `retry.rs`: jitter + `Retry-After`; `pty_read`: clamp `wait_ms`.
