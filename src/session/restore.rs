//! The read side of the session bundle: the L0–L9 ladder (verify **before**
//! parse), the orphan rules, the fail-closed decision tree, the pin rules, and
//! the N0–N6 normalization passes.
//!
//! **Verify before parse.** The manifest is the only authority on the member set
//! and on `byte_len`/`fingerprint`; a member that parses but whose length or
//! fingerprint disagrees is still a refusal — integrity is checked against the
//! raw bytes before serde ever sees them.
//!
//! **The loader is read-only with respect to disk.** No rewrite, no in-place
//! repair, no rename, no delete, no `.bak`: `Refused` leaves every byte for
//! post-mortem inspection and the refusal message says so ([`Refusal::display`]).
//! Normalization (N0–N6) is in-memory — the next *commit* writes the
//! normalized vector, not the restore.
//!
//! `RestoredSession` is **inert**: loading installs nothing into live state (L9);
//! [`load_bundle`] only decides, reports and refuses. The one tolerated partial:
//! exactly **one** corrupt or missing worker member while every other member
//! verifies ⇒ that worker comes back as [`WorkerStatus::Unrecoverable`] and
//! everything else restores ([`RestoreOutcome::PartiallyRestored`]). Two corrupt
//! members, or any manager or `ui.json` failure, is a refusal.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::agents::Agent;
use crate::manager::ContextEngine;
use crate::manager::context::{REBIRTH_CHECKPOINT_PREFIX, prune_orphan_tool_messages};
use crate::session::commit::{percent_unescape, worker_rel_path};
use crate::session::format::envelope::{
    ManagerFile, ManagerSection, Manifest, ManifestMember, PinWitness, ProcessEnding,
    SectionHeader, SectionKind,
};
use crate::session::format::messages::MessageRecord;
use crate::session::format::restore::{
    AppliedRule, NonRestorableKind, NonRestorableNotice, NormalizeReport, PinStage, Refusal,
    RefusalCode, RepairReport, RestoreOutcome, RestoredSession, WorkerRepairCounts, witness_of,
};
use crate::session::format::ui::{UiFile, UiSection};
use crate::session::format::worker::{
    DanglingCall, DanglingToolCallState, HistorySource, WorkerFile, WorkerSection, WorkerStatus,
};
use crate::session::format::{
    ABORTED_TOOL_RESULT, FORMAT_VERSION, MANAGER_FILE, MANIFEST_FILE, SCHEMA_VERSION, UI_FILE,
    WORKERS_DIR_NAME, fingerprint,
};

/// One verified+parsed member. A three-way option because `Loaded` is what
/// [`load_member`] hands back; the `kind` says which slot is filled.
#[derive(Debug)]
struct Loaded {
    kind: SectionKind,
    manager: Option<ManagerSection>,
    ui: Option<UiSection>,
    worker: Option<WorkerSection>,
}

/// N5: the pass set always runs twice — the second run is the stability
/// check, and it must be a no-op.
const PASS_SET_RUNS: u8 = 2;

/// The one call sites use. Walks L0–L9 and returns a verdict; never mutates a
/// byte of the bundle, never installs live state.
pub fn load_bundle(bundle: &Path) -> RestoreOutcome {
    let mut report = RepairReport::default();
    // A refusal is a first-class deliverable and it is never silent. The loader
    // writes the very sentence the UI shows; the report it returns is what feeds
    // the other two loud destinations (renderer status, journaled
    // `UiRecord::Status`), and none of the three may be a discarded `let _`.
    let refused = |report: RepairReport, refusal: Refusal| {
        tracing::warn!("{}", refusal.display(bundle));
        RestoreOutcome::Refused { refusal, report }
    };

    // ---------------------------------------------------------------- L0
    let manifest_file = bundle.join(MANIFEST_FILE);
    if !bundle.is_dir() || !manifest_file.is_file() {
        return no_bundle(bundle);
    }

    // ---------------------------------------------------------------- L1
    let manifest_bytes = match std::fs::read(&manifest_file) {
        Ok(bytes) => bytes,
        Err(err) => {
            return refused(
                report,
                Refusal::new(
                    RefusalCode::Io,
                    format!("cannot read {}: {err}", manifest_file.display()),
                )
                .with_member(MANIFEST_FILE),
            );
        }
    };

    // ---------------------------------------------------------------- L2
    let manifest: Manifest = match serde_json::from_slice(&manifest_bytes) {
        Ok(manifest) => manifest,
        Err(err) => {
            return refused(
                report,
                classify_parse_error(&err, MANIFEST_FILE, &manifest_bytes),
            );
        }
    };

    // ---------------------------------------------------------------- L3
    if manifest.format_version != FORMAT_VERSION {
        return refused(
            report,
            Refusal::new(
                RefusalCode::FormatVersion,
                format!(
                    "manifest format_version {} is not understood by this binary ({FORMAT_VERSION})",
                    manifest.format_version
                ),
            )
            .with_member(MANIFEST_FILE),
        );
    }
    if manifest.schema_version != SCHEMA_VERSION {
        return refused(
            report,
            Refusal::new(
                RefusalCode::SchemaVersion,
                format!(
                    "manifest schema_version {} is not understood by this binary ({SCHEMA_VERSION})",
                    manifest.schema_version
                ),
            )
            .with_member(MANIFEST_FILE),
        );
    }

    // A member file on disk that the manifest does not name is an orphan: ignored,
    // never merged, never partially read — but named out loud.
    let listed: BTreeMap<String, ()> = manifest
        .members
        .iter()
        .map(|m| (m.rel_path.clone(), ()))
        .collect();
    for orphan in orphan_members(bundle, &listed) {
        report.warnings.push(format!(
            "orphan member {orphan} is on disk but not listed in {MANIFEST_FILE}: ignored, \
             not merged (evidence of an interrupted overwrite)"
        ));
    }

    // ---------------------------------------------------------------- L4 + L5
    // Collected member outcomes. One worker member may fail on its own; a manager
    // or ui member failure, or a second worker failure, is a refusal. Failures are
    // kept in manifest order so the reported member is deterministic.
    let mut loaded: Vec<Loaded> = Vec::with_capacity(manifest.members.len());
    let mut failures: Vec<(SectionKind, String, Refusal)> = Vec::new();
    for member in &manifest.members {
        match load_member(bundle, member, &manifest) {
            Ok(entry) => loaded.push(entry),
            Err(refusal) => failures.push((member.kind, member.rel_path.clone(), refusal)),
        }
    }

    if let Some((_, _, refusal)) = failures
        .iter()
        .find(|(kind, _, _)| *kind != SectionKind::Worker)
    {
        return refused(report, refusal.clone());
    }

    let worker_failures: Vec<(&String, &Refusal)> = failures
        .iter()
        .filter(|(kind, _, _)| *kind == SectionKind::Worker)
        .map(|(_, path, refusal)| (path, refusal))
        .collect();

    if worker_failures.len() > 1 {
        let (path, refusal) = (worker_failures[0].0, worker_failures[0].1);
        let others: Vec<&str> = worker_failures[1..]
            .iter()
            .map(|(p, _)| p.as_str())
            .collect();
        return refused(
            report,
            Refusal::new(
                refusal.code,
                format!(
                    "{}; and {} more worker member(s) failed verification ({}), so the single \
                     corrupt-worker tolerance of A §7.5 does not apply",
                    refusal.message,
                    others.len(),
                    others.join(", ")
                ),
            )
            .with_member(path.clone()),
        );
    }

    let mut unrecoverable: Vec<WorkerSection> = Vec::new();
    if let Some((rel_path, refusal)) = worker_failures.first() {
        let key = worker_key_from_rel_path(rel_path);
        report
            .warnings
            .push(format!("worker member {rel_path}: {}", refusal.message));
        report.notices.push(NonRestorableNotice {
            kind: NonRestorableKind::WorkerHistory,
            subject: key.clone(),
            detail: format!(
                "worker member {rel_path} failed verification: {}",
                refusal.message
            ),
        });
        unrecoverable.push(unrecoverable_worker(&key, refusal));
    }

    // ------------------------------------------------------------- members
    let mut manager: Option<ManagerSection> = None;
    let mut ui: Option<UiSection> = None;
    let mut workers: Vec<WorkerSection> = Vec::new();
    for entry in loaded {
        match entry.kind {
            SectionKind::Manager => manager = entry.manager,
            SectionKind::Ui => ui = entry.ui,
            SectionKind::Worker => {
                if let Some(worker) = entry.worker {
                    workers.push(worker);
                }
            }
        }
    }
    workers.extend(unrecoverable);
    workers.sort_by(|a, b| a.worker_key.cmp(&b.worker_key));

    let Some(manager) = manager else {
        return refused(
            report,
            Refusal::new(
                RefusalCode::MissingMember,
                format!("{MANAGER_FILE} is listed by {MANIFEST_FILE} but was never loaded"),
            )
            .with_member(MANAGER_FILE),
        );
    };
    let ui = ui.unwrap_or_default();

    // ---------------------------------------------------------------- L6
    report.pinned_prefix_end_expected = Some(manager.pinned_prefix_end);
    report.pinned_prefix_end_actual = Some(pinned_prefix_end_records(&manager.messages) as u32);
    if let Err(refusal) = verify_pins(&manager, PinStage::Stored) {
        return refused(report, refusal);
    }
    report.pins_verified = true;

    // ---------------------------------------------------------------- L7
    if let Some(violation) = structural_violation(&manager, &ui, &workers, &manifest) {
        return refused(report, violation);
    }

    // ---------------------------------------------------------------- L8
    // Normalization only ever runs on a bundle that passed L0–L7: verify before
    // parse, refuse before repair. The passes are in-memory — the bundle bytes
    // stay exactly as they are, and the next commit writes the normalized vector
    // at the normal commit trigger, not here.
    let mut scratch = ContextEngine::new(manager.max_context_tokens.max(1));
    let pinned_before = pinned_prefix_end_records(&manager.messages);
    let normalized = match normalize(
        manager.messages.clone(),
        &manager.dangling,
        &manager.pin_witness,
        pinned_before,
        &mut scratch,
    ) {
        Ok((records, norm)) => {
            report.manager_messages_before = norm.messages_before;
            report.manager_messages_after = norm.messages_after;
            report.pinned_prefix_end_actual = Some(norm.pinned_prefix_end_after as u32);
            fold_pass_set(&mut report, MANAGER_FILE, "manager", &norm, &records, None);
            dispatched_dangling(&mut report, "manager", &manager.dangling);
            let mut section = manager.clone();
            section.messages = records;
            section
        }
        Err(refusal) => {
            // A normalization refusal is still a report. `passes` says how far the
            // pass set got and `normalization_stable: false` is what makes the
            // refusal a verdict rather than a warning.
            if refusal.code == RefusalCode::NormalizationUnstable {
                report.normalization_passes = PASS_SET_RUNS;
            }
            report.normalization_stable = false;
            return refused(report, refusal);
        }
    };
    // N3 in its normative form: the five pinned-prefix checks re-run on the
    // **installed** vector. L6 verified the stored one; this is what proves a pass
    // did not cost the session its pinned identity (that degradation is silent).
    if let Err(refusal) = verify_pins(&normalized, PinStage::Installed) {
        report.pins_verified = false;
        return refused(report, refusal);
    }
    report.pins_verified = true;

    for worker in &mut workers {
        if worker.status == WorkerStatus::Unrecoverable {
            continue;
        }
        let stored = std::mem::take(&mut worker.history);
        let mut worker_scratch = ContextEngine::new(worker.context_tokens.max(1));
        // A worker transcript has no pinned manager prefix: the witness is
        // recomputed from the worker's own first two messages, which is what
        // N3 can honestly verify.
        let witness = witness_of(&stored).unwrap_or(PinWitness {
            system_prompt_len: 0,
            goal_len: 0,
        });
        let subject = format!("worker {}", worker.worker_key);
        let member = worker_rel_path(&worker.worker_key);
        match normalize(
            stored.clone(),
            &worker.dangling,
            &witness,
            pinned_prefix_end_records(&stored),
            &mut worker_scratch,
        ) {
            Ok((records, norm)) => {
                fold_pass_set(
                    &mut report,
                    &member,
                    &subject,
                    &norm,
                    &records,
                    Some(&worker.worker_key),
                );
                dispatched_dangling(&mut report, &subject, &worker.dangling);
                worker.history = records;
            }
            Err(mut refusal) => {
                refusal.member = Some(member.clone());
                if refusal.code == RefusalCode::NormalizationUnstable {
                    report.normalization_passes = PASS_SET_RUNS;
                }
                report.normalization_stable = false;
                return refused(report, refusal);
            }
        }
    }

    // N6 witness cross-check: a disagreement is reported, never silently repaired.
    n6_witness_warnings(&mut report, &normalized, &manager, &workers);

    // ---------------------------------------------------------------- L9
    // Nothing above touched live state; nothing at all touched disk.
    report.ui_lines_restored = ui.journal.len();
    report.workers_restored = workers
        .iter()
        .filter(|w| w.status != WorkerStatus::Unrecoverable)
        .count();
    report.workers_unrecoverable = workers
        .iter()
        .filter(|w| w.status == WorkerStatus::Unrecoverable)
        .map(|w| w.worker_key.clone())
        .collect();

    // Repair and loss are never silent. The three loud destinations are the status
    // line, the journaled `UiRecord::Status` and `tracing::warn!`; the loader owns
    // the log and hands `report` (every sentence already in `warnings`, every
    // `notices` entry) to the other two. `let _ =` is prohibited.
    announce_verdict(&report, bundle);

    let ending = derive_ending(&manifest.ending);
    match report.workers_unrecoverable.clone() {
        keys if !keys.is_empty() => RestoreOutcome::PartiallyRestored {
            session: RestoredSession {
                session_id: manifest.session_id.clone(),
                manager: normalized,
                ui,
                workers,
                identity_floors: manifest.identity_floors,
                manifest,
                bundle: bundle.to_path_buf(),
            },
            ending,
            unrecoverable_workers: keys,
            report,
        },
        _ => RestoreOutcome::Restored {
            session: RestoredSession {
                session_id: manifest.session_id.clone(),
                manager: normalized,
                ui,
                workers,
                identity_floors: manifest.identity_floors,
                manifest,
                bundle: bundle.to_path_buf(),
            },
            ending,
            report,
        },
    }
}

/// The five pinned-prefix checks on a stored or installed `ManagerSection`.
///
/// `PinStage` only changes the *wording*: a stored-vector failure means the
/// bundle is inconsistent with itself; an installed-vector failure means a
/// normalization pass moved the pinned window — there is no *the repair moved
/// the window, accept it* branch.
pub fn verify_pins(section: &ManagerSection, on: PinStage) -> Result<(), Refusal> {
    let stage = on.label();
    if section.messages.len() < 2 {
        return Err(Refusal::new(
            RefusalCode::PinViolation,
            format!(
                "step 1: a {stage} transcript must hold at least the two pinned messages, \
                 it holds {}",
                section.messages.len()
            ),
        )
        .with_member(MANAGER_FILE));
    }

    let system_prompt_len = match &section.messages[0] {
        MessageRecord::System { content } => content.len(),
        other => {
            return Err(Refusal::new(
                RefusalCode::PinViolation,
                format!(
                    "step 2: messages[0] of a {stage} transcript must be the system prompt \
                     (REQ-CORE-001), it is a `{}` turn",
                    other.role()
                ),
            )
            .with_member(MANAGER_FILE));
        }
    };
    let goal_len = match &section.messages[1] {
        MessageRecord::User { content } => content.len(),
        other => {
            return Err(Refusal::new(
                RefusalCode::PinViolation,
                format!(
                    "step 3: messages[1] of a {stage} transcript must be the pinned goal \
                     (REQ-CORE-002), it is a `{}` turn",
                    other.role()
                ),
            )
            .with_member(MANAGER_FILE));
        }
    };

    // Step 5: the witness, before the window (so a broken pin names the pin).
    let witness = PinWitness {
        system_prompt_len: system_prompt_len as u32,
        goal_len: goal_len as u32,
    };
    if witness != section.pin_witness {
        return Err(Refusal::new(
            RefusalCode::PinViolation,
            format!(
                "step 5: pin witness mismatch on a {stage} transcript — stored system prompt \
                 {} bytes vs recorded {}, stored goal {} bytes vs recorded {}",
                witness.system_prompt_len,
                section.pin_witness.system_prompt_len,
                witness.goal_len,
                section.pin_witness.goal_len
            ),
        )
        .with_member(MANAGER_FILE));
    }

    // Step 4: recompute the pinned window and require the recorded value.
    let recomputed = pinned_prefix_end_records(&section.messages);
    if recomputed as u32 != section.pinned_prefix_end {
        return Err(Refusal::new(
            RefusalCode::PinViolation,
            format!(
                "step 4: pinned_prefix_end recomputed as {recomputed} on a {stage} transcript \
                 but the section records {}",
                section.pinned_prefix_end
            ),
        )
        .with_member(MANAGER_FILE));
    }
    Ok(())
}

/// N0–N6 for one transcript: manager and **every** worker vector go through this
/// same function, because `session/` owns no pairing rule. N1 is
/// [`prune_orphan_tool_messages`], N2 is `repair_tool_call_pairs` through the
/// engine seam [`ContextEngine::ensure_tool_call_pairs`] — the live loop's exact
/// grammar — N3 re-verifies the pins on the **installed** vector, N5 reruns the
/// passes for stability, N6 cross-checks the recorded witness. [`load_bundle`]
/// hands `engine` as a fresh scratch carrier per transcript, so a reused one can
/// never merge two transcripts, and nothing is ever written back to disk.
pub fn normalize(
    messages: Vec<MessageRecord>,
    recorded: &DanglingToolCallState,
    pins: &PinWitness,
    pinned_prefix_end: usize,
    engine: &mut ContextEngine,
) -> Result<(Vec<MessageRecord>, NormalizeReport), Refusal> {
    let messages_before = messages.len();
    let mut report = NormalizeReport {
        pruned_orphan_ids: Vec::new(),
        synthesized_aborted_ids: Vec::new(),
        regrouped_ids: Vec::new(),
        dropped_duplicate_ids: Vec::new(),
        applied: Vec::new(),
        n6_disagreements: Vec::new(),
        passes: 0,
        stable: false,
        pinned_prefix_end_before: pinned_prefix_end,
        pinned_prefix_end_after: pinned_prefix_end,
        messages_before,
        messages_after: messages_before,
    };

    // ------------------------------------------------------------- N0
    // Field by field. Prohibited on pain of rewriting history: any route through
    // serde, `ToolCall::new`, `sanitize_arguments` or
    // `ensure_valid_json_arguments` (`types/wire.rs`) — they mutate arguments.
    //
    // Byte-exactness is **asserted here, not assumed**: decode then re-encode must
    // be the identity for every record, or the pass set would be quietly rewriting
    // the `None`/`""` distinction it exists to protect.
    let stored = messages;
    let stored_positions = tool_result_positions(&stored);
    let mut wire: Vec<crate::types::Message> = stored
        .iter()
        .cloned()
        .map(MessageRecord::into_wire)
        .collect();
    if let Some(index) = decode_drift(&stored, &wire) {
        return Err(unstable(
            "N0/N4",
            format!(
                "decoding message {index} and re-encoding it is not the identity: the pass set \
                 would have rewritten a persisted record (`None` vs `Some(\"\")`, \
                 `reasoning_content` or verbatim `arguments`), so the transcript does not have \
                 one stable reading"
            ),
            &report,
        ));
    }

    // ------------------------------------------------------------- N1
    // N1 before N2 is not optional in reporting terms: `repair_tool_call_pairs`
    // prunes orphans internally, so calling only N2 would produce the right
    // transcript and the wrong report. Running N1 first is what makes an orphan
    // distinguishable from a dropped duplicate.
    let pruned = prune_orphan_tool_messages(std::mem::take(&mut wire));
    let after_prune = result_positions(&pruned);
    for (id, index) in dropped_results(&stored_positions, &after_prune) {
        report.pruned_orphan_ids.push(id.clone());
        report.applied.push(AppliedRule {
            rule: AppliedRule::N1_PRUNE,
            tool_call_id: id,
            index_before: Some(index),
            index_after: None,
        });
    }
    wire = pruned;

    // ------------------------------------------------------------- N2
    // The grammar itself is `manager::context::repair_tool_call_pairs`, reached
    // only through the documented idempotent seam: `session/` owns no pairing
    // rule, no grouping rule and no placeholder vocabulary of its own.
    drain(engine);
    for msg in wire.iter().cloned() {
        engine.append(msg);
    }
    let pre_repair = result_positions(&wire);
    wire.clear();
    let repaired = engine.ensure_tool_call_pairs();
    wire = engine.messages().to_vec();
    let healed = result_positions(&wire);
    if repaired {
        // Duplicate results for one id: N2 keeps the first and drops the rest.
        // The count must come out loud; `RepairReport` has no field for it, so it
        // is its own rule and its own warning sentence.
        for (id, index) in dropped_results(&pre_repair, &healed) {
            report.dropped_duplicate_ids.push(id.clone());
            report.applied.push(AppliedRule {
                rule: AppliedRule::N2_DEDUPLICATE,
                tool_call_id: id,
                index_before: Some(index),
                index_after: None,
            });
        }
        for (id, index) in added_results(&pre_repair, &healed) {
            report.synthesized_aborted_ids.push(id.clone());
            report.applied.push(AppliedRule {
                rule: AppliedRule::N2_SYNTHESIZE,
                tool_call_id: id,
                index_before: None,
                index_after: Some(index),
            });
        }
    }
    // Regrouping is counted against the un-normalized vector as stored, so the
    // index a reader can reproduce by hand is the one in the bundle.
    for (id, before, after) in moved_results(&stored_positions, &healed) {
        report.regrouped_ids.push(id.clone());
        report.applied.push(AppliedRule {
            rule: AppliedRule::N2_REGROUP,
            tool_call_id: id,
            index_before: Some(before),
            index_after: Some(after),
        });
    }
    report.passes = 1;
    report.messages_after = wire.len();

    // ------------------------------------------------------------- N3 + N4
    // N4 materializes the *installed* vector first: every N3 check below is
    // defined over the vector the loader would actually install, and that vector
    // is a record vector.
    let records: Vec<MessageRecord> = to_records(&wire);
    let pins_after = witness_of(&records).unwrap_or(PinWitness {
        system_prompt_len: 0,
        goal_len: 0,
    });
    if &pins_after != pins {
        return Err(Refusal::new(
            RefusalCode::PinViolation,
            format!(
                "N3: normalization changed a pinned message — witness is now system {} bytes / \
                 goal {} bytes, recorded system {} / goal {}",
                pins_after.system_prompt_len,
                pins_after.goal_len,
                pins.system_prompt_len,
                pins.goal_len
            ),
        )
        .with_member(MANAGER_FILE));
    }
    let window_after = pinned_prefix_end_records(&records);
    report.pinned_prefix_end_after = window_after;
    if window_after != pinned_prefix_end {
        return Err(Refusal::new(
            RefusalCode::PinViolation,
            format!(
                "N3: a normalization pass moved the pinned window: {pinned_prefix_end} -> \
                 {window_after} (pinned_prefix_end_expected {pinned_prefix_end}, \
                 pinned_prefix_end_actual {window_after}; A §8.2: there is no 'the repair moved \
                 the window, accept it' branch)"
            ),
        )
        .with_member(MANAGER_FILE));
    }
    // "`messages[0]` and `messages[1]` are never touched by any pass" is asserted
    // here, byte for byte, because losing the pinned prompt or goal fails silently.
    // Identity — not "same length" — is the claim.
    if let Some(detail) = pinned_identity_drift(&stored, &records, pinned_prefix_end) {
        return Err(
            Refusal::new(RefusalCode::PinViolation, format!("N3: {detail}"))
                .with_member(MANAGER_FILE),
        );
    }
    // The grammar prunes, moves and synthesizes **`Tool` messages only** — it
    // never edits content, never drops a turn and never re-roles or re-orders one
    // (advisory and checkpoint text is opaque). Asserting that is what makes "we
    // reused the manager grammar" checkable.
    if let Some(detail) = unpaired_message_drift(&stored, &records) {
        return Err(unstable("N4", detail, &report));
    }

    // ------------------------------------------------------------- N5
    // Run N1+N2 a second time: it must be a no-op. `ensure_tool_call_pairs`
    // documents idempotence; anything else means the input admitted two
    // incompatible readings — a verdict, not a retry.
    let mut second_wire: Vec<crate::types::Message> = records
        .iter()
        .cloned()
        .map(MessageRecord::into_wire)
        .collect();
    let second_before = result_positions(&second_wire);
    second_wire = prune_orphan_tool_messages(std::mem::take(&mut second_wire));
    let extra_pruned = dropped_results(&second_before, &result_positions(&second_wire));
    drain(engine);
    for msg in second_wire.into_iter() {
        engine.append(msg);
    }
    let changed = engine.ensure_tool_call_pairs();
    let second_records = to_records(engine.messages());
    let evidence = SecondPassEvidence {
        extra_pruned_ids: extra_pruned.iter().map(|(id, _)| id.clone()).collect(),
        changed,
        installed_drift: first_record_drift(&records, &second_records),
    };
    if let Some(refusal) = evidence.verdict(&report) {
        return Err(refusal);
    }
    report.passes = PASS_SET_RUNS;
    report.stable = true;

    // ------------------------------------------------------------- N6
    // The recorded witness is not re-healed. The caller compares the observed
    // state of the healed vector against the stored witness; this is the half only
    // the pass set can see — a capture that already claimed `Healed { ids }` for
    // an id N2 still had to synthesize means the member's witness and its
    // transcript disagree about what is in it. Such a disagreement is reported,
    // never silently repaired.
    if let DanglingToolCallState::Healed {
        aborted_tool_call_ids,
    } = recorded
    {
        for id in &report.synthesized_aborted_ids {
            if aborted_tool_call_ids.contains(id) {
                report.n6_disagreements.push(format!(
                    "N6: the recorded witness claims tool_call_id {id} was already healed with \
                     {ABORTED_TOOL_RESULT}, yet N2 found no result for it and synthesized one \
                     again — witness and transcript disagree; reported, not reconciled"
                ));
            }
        }
    }

    Ok((records, report))
}

/// Carries a refusal out to `main` so a `--resume` run can exit
/// [`super::EXIT_RESTORE_REFUSED`].
#[derive(Debug)]
pub struct RestoreRefused {
    pub refusal: Refusal,
    pub bundle: PathBuf,
}

impl std::fmt::Display for RestoreRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.refusal.display(&self.bundle))
    }
}

impl std::error::Error for RestoreRefused {}

/// The interim re-computation of `manager::context::pinned_prefix_end`
/// (**private**) over `MessageRecord`s.
///
/// The same rule with the same marker ([`REBIRTH_CHECKPOINT_PREFIX`]): the window
/// is `[0,1]` plus, if a checkpoint exists in `[2..]`, everything through the
/// **last** one. Both the `System` and the `User` spelling of the checkpoint are
/// matched on content, so a bundle written by an older build is read the same way.
pub fn pinned_prefix_end_records(messages: &[MessageRecord]) -> usize {
    messages
        .iter()
        .enumerate()
        .rposition(|(i, m)| {
            i >= 2
                && match m {
                    MessageRecord::System { content } | MessageRecord::User { content } => {
                        content.starts_with(REBIRTH_CHECKPOINT_PREFIX)
                    }
                    MessageRecord::Assistant { .. } | MessageRecord::Tool { .. } => false,
                }
        })
        .map(|idx| idx + 1)
        .unwrap_or(2)
}

/// The ending a user is told about: `InProgress` means the writer never recorded
/// its own death, so the loader **infers** `HardKillInferred`. The bundle bytes
/// are never rewritten to say so.
pub fn derive_ending(ending: &ProcessEnding) -> ProcessEnding {
    match ending {
        ProcessEnding::InProgress => ProcessEnding::HardKillInferred,
        other => other.clone(),
    }
}

// ------------------------------------------------------------------------- internals

/// **Legacy-only** journal names, spelled out as local literals.
///
/// Nothing writes either file: the session bundle is the **sole** persistence
/// owner. What still depends on these names is a user-facing promise — a fresh
/// boot with no bundle must NAME a stale copy left on disk by an older release
/// ("ignored" must not mean "invisible") so the user can delete it.
const LEGACY_MANAGER_JOURNAL: &str = ".session_transcript.json";
const LEGACY_UI_JOURNAL: &str = ".ui_transcript.json";

/// L0: no bundle. Not an error. Legacy journals and member-looking orphans are
/// named out loud — "ignored" must not mean "invisible".
fn no_bundle(bundle: &Path) -> RestoreOutcome {
    let mut legacy_journals = Vec::new();
    if let Some(parent) = bundle.parent() {
        for name in [LEGACY_MANAGER_JOURNAL, LEGACY_UI_JOURNAL] {
            let path = parent.join(name);
            if path.is_file() {
                legacy_journals.push(path);
            }
        }
    }
    RestoreOutcome::NoBundle {
        bundle: bundle.to_path_buf(),
        legacy_journals,
        orphan_members: orphan_members(bundle, &BTreeMap::new()),
    }
}

/// Files on disk that no manifest lists. Dot-prefixed entries are staging
/// leftovers — noise, not state, and never orphans.
fn orphan_members(bundle: &Path, listed: &BTreeMap<String, ()>) -> Vec<String> {
    let mut orphans = Vec::new();
    let mut scan = |dir: &Path, prefix: &str| {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }
            let rel = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}{name}")
            };
            // The manifest is the commit point, never a member, never an orphan.
            if rel == MANIFEST_FILE {
                continue;
            }
            if !listed.contains_key(&rel) {
                orphans.push(rel);
            }
        }
    };
    scan(bundle, "");
    scan(
        &bundle.join(WORKERS_DIR_NAME),
        &format!("{WORKERS_DIR_NAME}/"),
    );
    orphans.sort();
    orphans
}

/// L4 then L5 for a single member: integrity of the raw bytes **first**, parse
/// second, header cross-check third.
fn load_member(
    bundle: &Path,
    member: &ManifestMember,
    manifest: &Manifest,
) -> Result<Loaded, Refusal> {
    let path = bundle.join(&member.rel_path);
    let kind = member.kind;
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(Refusal::new(
                RefusalCode::MissingMember,
                format!(
                    "{} is listed by {MANIFEST_FILE} but is absent on disk (manifest says {} \
                     bytes, fingerprint 0x{:016x})",
                    member.rel_path, member.byte_len, member.fingerprint
                ),
            )
            .with_member(member.rel_path.clone()));
        }
        Err(err) => {
            return Err(Refusal::new(
                RefusalCode::Io,
                format!("cannot read {}: {err}", path.display()),
            )
            .with_member(member.rel_path.clone()));
        }
    };

    // L4: byte_len, then fingerprint. Both comparisons print both sides.
    if bytes.len() as u64 != member.byte_len {
        return Err(Refusal::new(
            RefusalCode::ByteLen,
            format!(
                "{}: manifest says {} bytes, the file on disk is {} bytes",
                member.rel_path,
                member.byte_len,
                bytes.len()
            ),
        )
        .with_member(member.rel_path.clone()));
    }
    let actual = fingerprint(&bytes);
    if actual != member.fingerprint {
        return Err(Refusal::new(
            RefusalCode::Fingerprint,
            format!(
                "{}: fingerprint 0x{:016x} != 0x{:016x} ({} bytes on both sides)",
                member.rel_path,
                member.fingerprint,
                actual,
                bytes.len()
            ),
        )
        .with_member(member.rel_path.clone()));
    }

    // L5: parse, then cross-check the header against the manifest.
    match kind {
        SectionKind::Manager => {
            let file: ManagerFile = parse_member(&bytes, &member.rel_path)?;
            check_header(&file.header, member, manifest, &member.rel_path)?;
            Ok(Loaded {
                kind,
                manager: Some(file.section),
                ui: None,
                worker: None,
            })
        }
        SectionKind::Ui => {
            let file: UiFile = parse_member(&bytes, &member.rel_path)?;
            check_header(&file.header, member, manifest, &member.rel_path)?;
            Ok(Loaded {
                kind,
                manager: None,
                ui: Some(file.section),
                worker: None,
            })
        }
        SectionKind::Worker => {
            let file: WorkerFile = parse_member(&bytes, &member.rel_path)?;
            check_header(&file.header, member, manifest, &member.rel_path)?;
            // The escaped path key is a *projection* of `worker_key`. A file whose
            // name decodes to something else is not this worker.
            let decoded = percent_unescape(file_stem(&member.rel_path))?;
            if decoded != file.section.worker_key {
                return Err(Refusal::new(
                    RefusalCode::MissingMember,
                    format!(
                        "{} decodes to worker key {decoded:?} but the section inside it says \
                         {:?}: the path is the authority, the file is not a member of this worker",
                        member.rel_path, file.section.worker_key
                    ),
                )
                .with_member(member.rel_path.clone()));
            }
            Ok(Loaded {
                kind,
                manager: None,
                ui: None,
                worker: Some(file.section),
            })
        }
    }
}

fn parse_member<T>(bytes: &[u8], rel_path: &str) -> Result<T, Refusal>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_slice(bytes).map_err(|err| classify_parse_error(&err, rel_path, bytes))
}

/// `deny_unknown_fields` violations are `UnknownField` (naming the field), an
/// unknown enum spelling or a torn document is `Json`, and the raw text of the
/// serde error — with line/column — is always carried.
fn classify_parse_error(err: &serde_json::Error, member: &str, bytes: &[u8]) -> Refusal {
    let text = err.to_string();
    let code = if text.contains("unknown field") {
        RefusalCode::UnknownField
    } else {
        RefusalCode::Json
    };
    Refusal::new(
        code,
        format!(
            "{member}: {text} (line {}, column {}; {} bytes read)",
            err.line(),
            err.column(),
            bytes.len()
        ),
    )
    .with_member(member)
}

/// L5 header cross-check: a header is a *witness*, never an input.
fn check_header(
    header: &SectionHeader,
    member: &ManifestMember,
    manifest: &Manifest,
    rel_path: &str,
) -> Result<(), Refusal> {
    let mut disagreements = Vec::new();
    if header.format_version != manifest.format_version {
        disagreements.push(format!(
            "format_version {} vs manifest {}",
            header.format_version, manifest.format_version
        ));
    }
    if header.schema_version != manifest.schema_version {
        disagreements.push(format!(
            "schema_version {} vs manifest {}",
            header.schema_version, manifest.schema_version
        ));
    }
    if header.session_id != manifest.session_id {
        disagreements.push(format!(
            "session_id {:?} vs manifest {:?}",
            header.session_id, manifest.session_id
        ));
    }
    if header.kind != member.kind {
        disagreements.push(format!(
            "kind {:?} vs manifest {:?}",
            header.kind, member.kind
        ));
    }
    if header.saved_wall != member.saved_wall {
        disagreements.push(format!(
            "saved_wall {} vs manifest {}",
            header.saved_wall.to_rfc3339(),
            member.saved_wall.to_rfc3339()
        ));
    }
    if header.saved_elapsed_secs.to_bits() != member.saved_elapsed_secs.to_bits() {
        disagreements.push(format!(
            "saved_elapsed_secs {} vs manifest {}",
            header.saved_elapsed_secs, member.saved_elapsed_secs
        ));
    }
    if header.member_count != 1 || header.member_index != 0 {
        disagreements.push(format!(
            "member_count/member_index {}/{} vs the one-member-per-file rule 1/0",
            header.member_count, header.member_index
        ));
    }
    if disagreements.is_empty() {
        return Ok(());
    }
    Err(Refusal::new(
        RefusalCode::HeaderManifestDisagreement,
        format!(
            "{rel_path} disagrees with {MANIFEST_FILE}: {}",
            disagreements.join("; ")
        ),
    )
    .with_member(rel_path))
}

/// L7: the structural requirements every section must satisfy.
fn structural_violation(
    manager: &ManagerSection,
    ui: &UiSection,
    workers: &[WorkerSection],
    manifest: &Manifest,
) -> Option<Refusal> {
    if manager.messages.len() < 2 {
        return Some(
            Refusal::new(
                RefusalCode::PinViolation,
                format!(
                    "L7: manager.json holds {} messages, fewer than the two pinned messages",
                    manager.messages.len()
                ),
            )
            .with_member(MANAGER_FILE),
        );
    }
    for worker in workers {
        for (field, point) in [
            ("started", Some(&worker.started)),
            ("completed", worker.completed.as_ref()),
            ("last_activity", worker.last_activity.as_ref()),
        ] {
            if let Some(point) = point
                && !point.is_sane()
            {
                return Some(
                    Refusal::new(
                        RefusalCode::HeaderManifestDisagreement,
                        format!(
                            "L7 (§5.6): worker {} {} is {} s — negative, NaN or infinite \
                             monotonic offsets are refused, never defaulted to now",
                            worker.worker_key, field, point.elapsed_secs
                        ),
                    )
                    .with_member(worker_rel_path(&worker.worker_key)),
                );
            }
        }
    }
    let violations = ui.structural_violations();
    if !violations.is_empty() {
        return Some(
            Refusal::new(
                RefusalCode::HeaderManifestDisagreement,
                format!("L7 (§6.5): ui.json: {}", violation_text(&violations)),
            )
            .with_member(UI_FILE),
        );
    }
    let floors = manifest.identity_floors.sanity_violations(workers);
    if !floors.is_empty() {
        return Some(
            Refusal::new(
                RefusalCode::HeaderManifestDisagreement,
                format!("L7 (§5.7): {}", violation_text(&floors)),
            )
            .with_member(MANIFEST_FILE),
        );
    }
    None
}

/// One refusal sentence for N structural violations: every one is named,
/// because a reader must be able to fix the file by hand from the message alone.
fn violation_text(violations: &[String]) -> String {
    violations.join("; ")
}

/// N6: compare the recorded pairing witness against what the transcript actually
/// contains after N1/N2. A disagreement is reported, never silently repaired.
fn n6_witness_warnings(
    report: &mut RepairReport,
    normalized: &ManagerSection,
    stored: &ManagerSection,
    workers: &[WorkerSection],
) {
    let observed = DanglingToolCallState::observe(&normalized.messages);
    if observed != stored.dangling {
        report.warnings.push(format!(
            "manager.json recorded dangling state {} but N1/N2 left {}: {} rule(s) applied, \
             reported not reconciled",
            describe_dangling(&stored.dangling),
            describe_dangling(&observed),
            report.orphan_tool_results_pruned
                + report.aborted_results_synthesized
                + report.tool_results_regrouped
        ));
    }
    if normalized.pinned_prefix_end != stored.pinned_prefix_end {
        report.warnings.push(format!(
            "manager.json pinned_prefix_end {} -> {} after normalization",
            stored.pinned_prefix_end, normalized.pinned_prefix_end
        ));
    }
    for worker in workers {
        if worker.status == WorkerStatus::Unrecoverable {
            continue;
        }
        let observed = DanglingToolCallState::observe(&worker.history);
        if observed != worker.dangling {
            report.warnings.push(format!(
                "worker {} recorded dangling state {} but N1/N2 left {}",
                worker.worker_key,
                describe_dangling(&worker.dangling),
                describe_dangling(&observed)
            ));
        }
    }
}

fn describe_dangling(state: &DanglingToolCallState) -> String {
    match state {
        DanglingToolCallState::Balanced => "balanced".to_string(),
        DanglingToolCallState::Dangling { calls } => format!(
            "dangling {} call(s): {}",
            calls.len(),
            calls
                .iter()
                .map(|c| c.tool_call_id.clone())
                .collect::<Vec<_>>()
                .join(",")
        ),
        DanglingToolCallState::OrphanedResults { tool_call_ids } => {
            format!("orphaned results {}", tool_call_ids.join(","))
        }
        DanglingToolCallState::Healed {
            aborted_tool_call_ids,
        } => {
            format!("healed {}", aborted_tool_call_ids.join(","))
        }
    }
}

/// The partial-restore stand-in for a worker whose member failed verification.
/// Its history is unrecoverable; its *pane identity* is kept so the UI can still
/// show that the worker existed, and the reason is carried verbatim.
fn unrecoverable_worker(key: &str, refusal: &Refusal) -> WorkerSection {
    WorkerSection {
        worker_key: key.to_string(),
        worker_id_seq_value: 0,
        role: role_hint_from_key(key),
        task_id: task_hint_from_key(key),
        status: WorkerStatus::Unrecoverable,
        status_label: "unrecoverable".to_string(),
        started: crate::session::format::TimePoint {
            elapsed_secs: 0.0,
            wall: chrono::Local::now(),
        },
        completed: None,
        last_activity: None,
        worked_secs: 0.0,
        completed_seq: None,
        context_tokens: 0,
        last_tokens: 0,
        counters: Default::default(),
        brief: String::new(),
        snippets: Vec::new(),
        image_urls: Vec::new(),
        audio_urls: Vec::new(),
        recursion_granted: false,
        history: Vec::new(),
        history_source: HistorySource::NotCaptured,
        dangling: DanglingToolCallState::Balanced,
        unrecoverable_reason: Some(refusal.message.clone()),
        non_restorable: vec![NonRestorableNotice {
            kind: NonRestorableKind::WorkerHistory,
            subject: key.to_string(),
            detail: refusal.message.clone(),
        }],
    }
}

/// `{agent_name}-{task_id}` / `{agent_name}-w{worker_id}`: the agent name is the
/// head of the key, so an unrecoverable worker can still be classified without
/// inventing an identity. An unrecognized head is `Generalist` and the reason text
/// says the role is a guess from the key, never a recovered fact.
fn role_hint_from_key(key: &str) -> Agent {
    let head = key.split(['-', '/', '#']).next().unwrap_or("");
    Agent::from_str(head).unwrap_or(Agent::Generalist)
}

fn task_hint_from_key(key: &str) -> Option<String> {
    let head = key.split(['-', '/', '#']).next().unwrap_or("");
    let rest = key.strip_prefix(head)?.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('w') && rest[1..].chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(rest.to_string())
}

/// The worker key a `rel_path` promises, best effort: a name that does not decode
/// keeps its raw stem so the UI still names the thing it is talking about.
fn worker_key_from_rel_path(rel_path: &str) -> String {
    let stem = file_stem(rel_path);
    percent_unescape(stem).unwrap_or_else(|_| stem.to_string())
}

fn file_stem(rel_path: &str) -> &str {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    name.strip_suffix(".json").unwrap_or(name)
}

/// The per-member rule summary for [`WorkerRepairCounts`] — the same facts the
/// `warnings` sentences carry, structured where a test can assert them.
fn applied_rules(report: &NormalizeReport) -> Vec<String> {
    let mut rules = Vec::new();
    if !report.pruned_orphan_ids.is_empty() {
        rules.push(format!(
            "N1 pruned {} orphan tool result(s): {}",
            report.pruned_orphan_ids.len(),
            report.pruned_orphan_ids.join(",")
        ));
    }
    if !report.synthesized_aborted_ids.is_empty() {
        rules.push(format!(
            "N2 synthesized {} {ABORTED_TOOL_RESULT} tool result(s): {}",
            report.synthesized_aborted_ids.len(),
            report.synthesized_aborted_ids.join(",")
        ));
    }
    if !report.regrouped_ids.is_empty() {
        rules.push(format!(
            "N2 regrouped {} tool result(s): {}",
            report.regrouped_ids.len(),
            report.regrouped_ids.join(",")
        ));
    }
    if !report.dropped_duplicate_ids.is_empty() {
        rules.push(format!(
            "N2 dropped {} duplicate tool result(s): {}",
            report.dropped_duplicate_ids.len(),
            report.dropped_duplicate_ids.join(",")
        ));
    }
    rules
}

/// Empties a scratch carrier without dropping any rule (see [`normalize`]).
fn drain(engine: &mut ContextEngine) {
    while engine.pop_last().is_some() {}
}

fn to_records(wire: &[crate::types::Message]) -> Vec<MessageRecord> {
    wire.iter().map(MessageRecord::from_wire).collect()
}

/// `(tool_call_id, index)` of every `role:"tool"` **wire** message — enough to
/// tell "moved" from "added" from "gone" without touching a byte of content.
fn result_positions(wire: &[crate::types::Message]) -> Vec<(String, usize)> {
    wire.iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            crate::types::Message::Tool { tool_call_id, .. } => Some((tool_call_id.clone(), i)),
            _ => None,
        })
        .collect()
}

/// The same over `MessageRecord`s, for the un-normalized baseline the regrouping
/// count is measured against.
fn tool_result_positions(messages: &[MessageRecord]) -> Vec<(String, usize)> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            MessageRecord::Tool { tool_call_id, .. } => Some((tool_call_id.clone(), i)),
            _ => None,
        })
        .collect()
}

/// Results `before` carries that `after` does not — multiplicity counted, so a
/// dropped duplicate is reported exactly once — keeping the index each one had
/// in `before`, which is the index the report names.
fn dropped_results(before: &[(String, usize)], after: &[(String, usize)]) -> Vec<(String, usize)> {
    let mut remaining: Vec<&(String, usize)> = after.iter().collect();
    let mut dropped = Vec::new();
    for entry in before {
        match remaining.iter().position(|other| other.0 == entry.0) {
            Some(index) => {
                remaining.remove(index);
            }
            None => dropped.push(entry.clone()),
        }
    }
    dropped
}

/// Results `after` carries that `before` did not: exactly the placeholders N2
/// synthesized for `tool_calls` that had no result.
fn added_results(before: &[(String, usize)], after: &[(String, usize)]) -> Vec<(String, usize)> {
    let mut seen: Vec<&(String, usize)> = before.iter().collect();
    let mut added = Vec::new();
    for entry in after {
        match seen.iter().position(|other| other.0 == entry.0) {
            Some(index) => {
                seen.remove(index);
            }
            None => added.push(entry.clone()),
        }
    }
    added
}

/// Results that are in both vectors at a **different** index — the "moved under
/// their parent assistant" half of `repair_tool_call_pairs`.
fn moved_results(
    before: &[(String, usize)],
    after: &[(String, usize)],
) -> Vec<(String, usize, usize)> {
    let mut pool: Vec<&(String, usize)> = after.iter().collect();
    let mut moved = Vec::new();
    for entry in before {
        if let Some(slot) = pool.iter().position(|other| other.0 == entry.0) {
            let landed = pool.remove(slot);
            if landed.1 != entry.1 {
                moved.push((entry.0.clone(), entry.1, landed.1));
            }
        }
    }
    moved
}

/// N0/N4 byte-exactness, **asserted not assumed**: decoding a record into the
/// wire type and re-encoding it must return the same record. This is where a
/// regression that folds `content: None` into `Some("")`, drops
/// `reasoning_content` or re-canonicalizes `arguments` is caught, instead of
/// surfacing as a provider 400 or a silently rewritten transcript.
fn decode_drift(stored: &[MessageRecord], wire: &[crate::types::Message]) -> Option<usize> {
    match stored
        .iter()
        .zip(wire.iter())
        .position(|(record, msg)| MessageRecord::from_wire(msg) != *record)
    {
        Some(index) => Some(index),
        None if stored.len() != wire.len() => Some(stored.len().min(wire.len())),
        None => None,
    }
}

/// Where two record vectors first differ, as an index a reader can look up in the
/// bundle (the N5 comparison prints it).
fn first_record_drift(before: &[MessageRecord], after: &[MessageRecord]) -> Option<usize> {
    match before.iter().zip(after.iter()).position(|(a, b)| a != b) {
        Some(index) => Some(index),
        None if before.len() != after.len() => Some(before.len().min(after.len())),
        None => None,
    }
}

/// Invariant asserted rather than trusted: the reused grammar prunes, moves and
/// synthesizes **`Tool` messages only**. Every other turn must come back identical
/// and in the same order — no reworded advisory or checkpoint text, no re-roled
/// or re-ordered turn, no dropped assistant turn.
fn unpaired_message_drift(before: &[MessageRecord], after: &[MessageRecord]) -> Option<String> {
    let before_kept = non_tool_turns(before);
    let after_kept = non_tool_turns(after);
    if before_kept.len() != after_kept.len() {
        return Some(format!(
            "the pass set changed the number of non-tool messages {} -> {} (vector {} -> {}); \
             the reused grammar prunes, moves and synthesizes tool results and touches nothing \
             else (manager/context.rs:441-443)",
            before_kept.len(),
            after_kept.len(),
            before.len(),
            after.len()
        ));
    }
    for (index, (was, now)) in before_kept.iter().zip(after_kept.iter()).enumerate() {
        if was != now {
            return Some(format!(
                "the pass set rewrote or re-ordered non-tool message #{index} (`{}` -> `{}`); \
                 advisory and checkpoint text is opaque and turns are never re-roled or \
                 re-ordered (A §4 R-PIN-3/R-PIN-4)",
                was.role(),
                now.role()
            ));
        }
    }
    None
}

/// N3 identity assertion. `messages[0]` (the system prompt) and `messages[1]`
/// (the pinned goal) must survive **byte-identically**, keep their roles, and
/// every non-`Tool` turn inside the pinned window must be unchanged. A pruned or
/// moved `Tool` result inside the window is legal; a reworded checkpoint or a
/// replaced goal is a pin violation, not a repair.
fn pinned_identity_drift(
    before: &[MessageRecord],
    after: &[MessageRecord],
    window: usize,
) -> Option<String> {
    // A transcript that never had the pinned pair cannot lose it. For the manager
    // this state is impossible — `verify_pins` step 1 refuses a transcript with
    // fewer than two messages at both L6 and the installed re-verification — so
    // this is the worker whose history is empty (it never ran), and there is no
    // pin there to violate.
    if before.len() < 2 {
        return None;
    }
    if after.len() < 2 {
        return Some(format!(
            "the pinned region needs at least the two pinned messages; the pass set left {} \
             message(s) out of {}",
            after.len(),
            before.len()
        ));
    }
    for index in 0..2 {
        if before.get(index) != after.get(index) {
            return Some(format!(
                "messages[{index}] is no longer byte-identical after normalization (`{}` -> \
                 `{}`): the pinned {} was rewritten by a pass (REQ-CORE-00{})",
                before
                    .get(index)
                    .map_or("nothing".to_string(), |m| m.role().to_string()),
                after[index].role(),
                if index == 0 { "system prompt" } else { "goal" },
                index + 1
            ));
        }
    }
    // Byte-identity already fixes the roles, so this can only fire on a stored
    // transcript that already had the pinned shape — asserted for the manager
    // because losing the pinned prompt or goal fails silently. A worker transcript
    // that legitimately starts differently is not rewritten by that claim.
    if matches!(
        (&before[0], &before[1]),
        (MessageRecord::System { .. }, MessageRecord::User { .. })
    ) && !matches!(
        (&after[0], &after[1]),
        (MessageRecord::System { .. }, MessageRecord::User { .. })
    ) {
        return Some(format!(
            "messages[0]/messages[1] of the installed transcript are `{}`/`{}` but must be \
             system/user (REQ-CORE-001/002)",
            after[0].role(),
            after[1].role()
        ));
    }
    if pinned_non_tool_turns(before, window) != pinned_non_tool_turns(after, window) {
        return Some(format!(
            "the pinned window [{}, {}) lost or rewrote a turn across the pass set",
            0, window
        ));
    }
    None
}

/// The non-`Tool` turns of a transcript, in order. The reused grammar prunes,
/// moves and synthesizes `Tool` messages and touches nothing else, so this
/// subsequence is what a pass set provably left alone.
fn non_tool_turns(messages: &[MessageRecord]) -> Vec<&MessageRecord> {
    messages
        .iter()
        .filter(|m| !matches!(m, MessageRecord::Tool { .. }))
        .collect()
}

/// The same, restricted to the pinned window `[0, window)`.
fn pinned_non_tool_turns(messages: &[MessageRecord], window: usize) -> Vec<&MessageRecord> {
    messages
        .iter()
        .take(window)
        .filter(|m| !matches!(m, MessageRecord::Tool { .. }))
        .collect()
}

/// What the second N5 run produced. Nothing means the pass set is a
/// fixed point and the transcript has exactly one reading.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SecondPassEvidence {
    /// Tool results the second N1 pruned although the first pass set had already
    /// run N1.
    extra_pruned_ids: Vec<String>,
    /// The second `ensure_tool_call_pairs` reported a change.
    changed: bool,
    /// First message the second pass set moved, added or reworded.
    installed_drift: Option<usize>,
}

impl SecondPassEvidence {
    /// Non-idempotence is a **verdict**, not a retry. The loader SHALL NOT loop to
    /// a fixed point, SHALL NOT pick one reading and SHALL NOT persist a normalized
    /// copy — `Refused { NormalizationUnstable }`, bundle bytes untouched, both
    /// pass reports printed.
    fn verdict(&self, report: &NormalizeReport) -> Option<Refusal> {
        let mut findings = Vec::new();
        if !self.extra_pruned_ids.is_empty() {
            findings.push(format!(
                "the second N1 pass pruned {} more tool result(s): {}",
                self.extra_pruned_ids.len(),
                self.extra_pruned_ids.join(",")
            ));
        }
        if self.changed {
            findings.push(
                "the second `ensure_tool_call_pairs` pass changed the transcript, breaking the \
                 idempotence that pass is documented with (manager/context.rs:887-890)"
                    .to_string(),
            );
        }
        if let Some(index) = self.installed_drift {
            findings.push(format!(
                "the installed vector and the re-passed vector differ at message {index}"
            ));
        }
        if findings.is_empty() {
            return None;
        }
        Some(unstable("N5", findings.join("; "), report))
    }
}

/// The one `NormalizationUnstable` refusal shape: both pass reports are printed,
/// so a reader can check the numbers by hand.
fn unstable(rule: &str, detail: impl Into<String>, report: &NormalizeReport) -> Refusal {
    Refusal::new(
        RefusalCode::NormalizationUnstable,
        format!(
            "{rule}: {} — pass 1 applied {} rule(s) over {} message(s) ({} pruned, {} \
             synthesized, {} regrouped, {} duplicate(s) dropped) and pass 2 did not leave them \
             alone, so the input admitted two incompatible readings; the loader refuses: it does \
             not loop to a fixed point, does not choose a reading, and never persists a \
             normalized copy (A §8.4)",
            detail.into(),
            report.applied.len(),
            report.messages_before,
            report.pruned_orphan_ids.len(),
            report.synthesized_aborted_ids.len(),
            report.regrouped_ids.len(),
            report.dropped_duplicate_ids.len(),
        ),
    )
    .with_member(MANAGER_FILE)
}

/// The loudness sentence for one applied rule: the rule, the `tool_call_id`, and
/// the message index it acted on. For a synthesized placeholder the sentence
/// carries the function name and the verbatim arguments of the call that never got
/// an answer, so a user can see what the model asked for.
fn rule_sentence(rule: &AppliedRule, subject: &str, healed: &[MessageRecord]) -> String {
    let index = |value: Option<usize>| match value {
        Some(value) => value.to_string(),
        None => "-".to_string(),
    };
    if rule.rule == AppliedRule::N1_PRUNE {
        return format!(
            "pruned orphan tool result {} at message index {} ({subject})",
            rule.tool_call_id,
            index(rule.index_before)
        );
    }
    if rule.rule == AppliedRule::N2_DEDUPLICATE {
        return format!(
            "dropped duplicate tool result {} at message index {} (kept the first) ({subject})",
            rule.tool_call_id,
            index(rule.index_before)
        );
    }
    if rule.rule == AppliedRule::N2_REGROUP {
        return format!(
            "regrouped tool result {} from message index {} to {} ({subject})",
            rule.tool_call_id,
            index(rule.index_before),
            index(rule.index_after)
        );
    }
    let (function, arguments) = call_of(healed, &rule.tool_call_id);
    format!(
        "synthesized {ABORTED_TOOL_RESULT} for tool_call_id {} (function {function}, {subject}): \
         arguments {arguments}",
        rule.tool_call_id
    )
}

/// The call a synthesized result belongs to, read out of the healed vector: that
/// is where the assistant turn carrying it now lives. Verbatim arguments only —
/// never a re-parse (`ToolFunctionRecord::is_malformed` is a read-out, a re-write
/// would falsify what the model produced).
fn call_of(records: &[MessageRecord], id: &str) -> (String, String) {
    for record in records {
        if let MessageRecord::Assistant { tool_calls, .. } = record {
            for call in tool_calls {
                if call.id == id {
                    return (call.function.name.clone(), call.function.arguments.clone());
                }
            }
        }
    }
    ("<unknown>".to_string(), "<unknown>".to_string())
}

/// Repair is never silent. The live boundary repair already writes a
/// `tracing::warn!`, a renderer status and a journaled `UiRecord::Status`; the
/// loader owns the first and hands the report to the other two. The sentence keeps
/// the live wording's shape with the site spelled `"on session restore"` and the
/// member named, so a user who saw the live message recognizes this one.
fn repair_sentence(member: &str, norm: &NormalizeReport) -> String {
    format!(
        "tool-call pairing repaired (on session restore): missing tool results synthesized as \
         `{ABORTED_TOOL_RESULT}` — {member}: pruned {} orphan result(s), synthesized {} \
         placeholder(s), regrouped {}, dropped {} duplicate(s); history {} -> {} message(s); \
         the bundle was NOT rewritten, the next commit writes the normalized vector",
        norm.pruned_orphan_ids.len(),
        norm.synthesized_aborted_ids.len(),
        norm.regrouped_ids.len(),
        norm.dropped_duplicate_ids.len(),
        norm.messages_before,
        norm.messages_after
    )
}

/// The loud half of the loader: one `warn!` per member the passes changed, plus
/// one line per applied rule so the log alone tells the whole story.
fn announce_repairs(member: &str, subject: &str, norm: &NormalizeReport, healed: &[MessageRecord]) {
    if norm.applied.is_empty() {
        return;
    }
    tracing::warn!("{}", repair_sentence(member, norm));
    for rule in &norm.applied {
        tracing::warn!("{}", rule_sentence(rule, subject, healed));
    }
}

/// The `NonRestorableNotice` required for `dispatched: true` dangling calls: the
/// tool may have run and mutated the workspace while its result never landed, and
/// the loader can claim neither that it did nor that it did not.
fn dispatched_notice(member: &str, calls: &[&DanglingCall]) -> NonRestorableNotice {
    let list = calls
        .iter()
        .map(|call| {
            format!(
                "tool_call_id {} (function {}, arguments {})",
                call.tool_call_id, call.function_name, call.arguments
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    NonRestorableNotice {
        kind: NonRestorableKind::PtyProcess,
        subject: member.to_string(),
        detail: format!(
            "{list} had already been dispatched when the process died and its result never \
             landed, so the tool may have run and changed the workspace while its output is gone. \
             The loader will not re-run it and will not assume the effect did not happen: check \
             the workspace if these calls mutated it."
        ),
    }
}

/// An already-dispatched dangling call is a lost capability, not a repair, so it
/// is a notice and not a warning. Deduplicated on `(kind, subject)`, and every id
/// is still named in the sentence so the report stays checkable by hand.
fn dispatched_dangling(report: &mut RepairReport, subject: &str, recorded: &DanglingToolCallState) {
    let DanglingToolCallState::Dangling { calls } = recorded else {
        return;
    };
    let dispatched: Vec<&DanglingCall> = calls.iter().filter(|call| call.dispatched).collect();
    if dispatched.is_empty() {
        return;
    }
    for call in &dispatched {
        report.warnings.push(format!(
            "tool_call_id {} (function {}) was dispatched but never answered: its result is gone \
             and the loader will not re-run it ({subject})",
            call.tool_call_id, call.function_name
        ));
    }
    let notice = dispatched_notice(subject, &dispatched);
    let key = notice.dedup_key();
    if !report.notices.iter().any(|n| n.dedup_key() == key) {
        report.notices.push(notice);
    }
}

/// Fold one member's pass-set report into the loader's [`RepairReport`] — the
/// counts, one warning sentence per applied rule (rule, `tool_call_id`, affected
/// index), the per-worker before/after counts, and the N6 disagreements. Repair is
/// never silent, so this is not optional on any path that ran the passes.
fn fold_pass_set(
    report: &mut RepairReport,
    member: &str,
    subject: &str,
    norm: &NormalizeReport,
    healed: &[MessageRecord],
    worker_key: Option<&str>,
) {
    report.orphan_tool_results_pruned += norm.pruned_orphan_ids.len();
    report.aborted_results_synthesized += norm.synthesized_aborted_ids.len();
    report.tool_results_regrouped += norm.regrouped_ids.len();
    if report.normalization_passes == 0 {
        report.normalization_passes = norm.passes;
        report.normalization_stable = norm.stable;
    } else {
        report.normalization_passes = report.normalization_passes.max(norm.passes);
        report.normalization_stable = report.normalization_stable && norm.stable;
    }
    report
        .warnings
        .extend(norm.n6_disagreements.iter().cloned());
    for rule in &norm.applied {
        report.warnings.push(rule_sentence(rule, subject, healed));
    }
    if let Some(key) = worker_key {
        report.worker_counts.push(WorkerRepairCounts {
            key: key.to_string(),
            before: norm.messages_before,
            after: norm.messages_after,
            applied_rules: applied_rules(norm),
        });
        // The per-worker numbers live in `warnings` as a sentence, in the exact
        // shape `worker <key>: history <before> -> <after>`.
        report.warnings.push(format!(
            "worker {key}: history {} -> {}",
            norm.messages_before, norm.messages_after
        ));
    }
    announce_repairs(member, subject, norm, healed);
}

/// The verdict line: one summary `warn!` whenever a pass changed a transcript, one
/// per `NonRestorableNotice`, and an explicit verdict line so a clean restore is
/// distinguishable from a silent no-op in the log. The counts repeat the numbers in
/// `RepairReport`, and the sentence repeats the one promise that matters most: the
/// bundle bytes were not rewritten.
fn announce_verdict(report: &RepairReport, bundle: &Path) {
    let changed = report.orphan_tool_results_pruned
        + report.aborted_results_synthesized
        + report.tool_results_regrouped;
    if changed > 0 {
        tracing::warn!(
            "tool-call pairing repaired (on session restore): missing tool results synthesized \
             as `{ABORTED_TOOL_RESULT}` — bundle {}: manager history {} -> {} message(s), \
             worker transcript(s) normalized {}, pruned {} orphan result(s), synthesized {} \
             placeholder(s), regrouped {} result(s), passes {}, stable {}; the bundle was NOT \
             rewritten, the next commit writes the normalized vector",
            bundle.display(),
            report.manager_messages_before,
            report.manager_messages_after,
            report.worker_counts.len(),
            report.orphan_tool_results_pruned,
            report.aborted_results_synthesized,
            report.tool_results_regrouped,
            report.normalization_passes,
            report.normalization_stable,
        );
    }
    for notice in &report.notices {
        tracing::warn!(
            "not restorable after restore [{:?}] {}: {}",
            notice.kind,
            notice.subject,
            notice.detail
        );
    }
    if !report.workers_unrecoverable.is_empty() {
        tracing::warn!(
            "session restore partially restored {}: worker history is gone for {} — the bundle \
             was NOT modified",
            bundle.display(),
            report.workers_unrecoverable.join(", ")
        );
    } else if changed == 0 {
        tracing::info!(
            "session restore: {} normalized cleanly (passes {}, pins verified {}, {} worker(s)); \
             no tool-call pairing repair was needed",
            bundle.display(),
            report.normalization_passes,
            report.pins_verified,
            report.workers_restored,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::commit::{CapturePayload, CommitLock, CommitPolicy, write_bundle};
    use crate::session::format::TimePoint;
    use crate::session::format::envelope::IdentityFloors;
    use crate::session::format::messages::{ToolCallRecord, ToolFunctionRecord};
    use crate::tool_names::{TOOL_GLOB, TOOL_READ_FILE, TOOL_REPLACE, TOOL_RUN_COMMAND};

    // ----------------------------------------------------------------- fixtures

    fn sys(content: &str) -> MessageRecord {
        MessageRecord::System {
            content: content.to_string(),
        }
    }

    fn user(content: &str) -> MessageRecord {
        MessageRecord::User {
            content: content.to_string(),
        }
    }

    /// A rebirth checkpoint, spelled exactly like the live injection so
    /// [`pinned_prefix_end_records`] pins it the way the manager does.
    fn checkpoint(body: &str) -> MessageRecord {
        user(&format!("{REBIRTH_CHECKPOINT_PREFIX}{body}"))
    }

    fn call(id: &str, name: &str, arguments: &str) -> ToolCallRecord {
        ToolCallRecord {
            id: id.to_string(),
            kind: "function".to_string(),
            function: ToolFunctionRecord {
                name: name.to_string(),
                arguments: arguments.to_string(),
            },
        }
    }

    fn asst(content: Option<&str>, calls: &[ToolCallRecord]) -> MessageRecord {
        MessageRecord::Assistant {
            content: content.map(str::to_string),
            reasoning_content: None,
            tool_calls: calls.to_vec(),
        }
    }

    fn tool(id: &str, content: &str) -> MessageRecord {
        MessageRecord::Tool {
            tool_call_id: id.to_string(),
            content: content.to_string(),
        }
    }

    fn section(messages: Vec<MessageRecord>) -> ManagerSection {
        ManagerSection {
            pinned_prefix_end: pinned_prefix_end_records(&messages) as u32,
            pin_witness: PinWitness {
                system_prompt_len: messages
                    .first()
                    .map(|m| m.content().map_or(0, |c| c.len()))
                    .unwrap_or(0) as u32,
                goal_len: messages
                    .get(1)
                    .map(|m| m.content().map_or(0, |c| c.len()))
                    .unwrap_or(0) as u32,
            },
            max_context_tokens: 128,
            rebirth_advisory_emitted: false,
            consecutive_rebirths: 0,
            dangling: DanglingToolCallState::observe(&messages),
            messages,
        }
    }

    /// Run the pass set the way the loader does: recorded state as a capture would
    /// have written it, witness and window recomputed from the stored vector.
    fn pass(
        messages: Vec<MessageRecord>,
    ) -> Result<(Vec<MessageRecord>, NormalizeReport), Refusal> {
        let recorded = DanglingToolCallState::observe(&messages);
        pass_with(messages, recorded)
    }

    fn pass_with(
        messages: Vec<MessageRecord>,
        recorded: DanglingToolCallState,
    ) -> Result<(Vec<MessageRecord>, NormalizeReport), Refusal> {
        let pins = witness_of(&messages).expect("fixture must start system/user");
        let window = pinned_prefix_end_records(&messages);
        let mut engine = ContextEngine::new(1024);
        normalize(messages, &recorded, &pins, window, &mut engine)
    }

    fn tool_id(record: &MessageRecord) -> Option<String> {
        match record {
            MessageRecord::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        }
    }

    /// The provider contract the pass set protects: every `tool_calls` entry is
    /// answered immediately after its parent, in call order, and no `Tool` result
    /// outlives a surviving parent.
    fn assert_sendable(records: &[MessageRecord]) {
        let mut parents: Vec<String> = Vec::new();
        let mut index = 0;
        while index < records.len() {
            match &records[index] {
                MessageRecord::Assistant { tool_calls, .. } => {
                    for (offset, entry) in tool_calls.iter().enumerate() {
                        let answered = records.get(index + 1 + offset);
                        assert_eq!(
                            answered.map(|record| (record.role(), tool_id(record))),
                            Some(("tool", Some(entry.id.clone()))),
                            "tool_call_id {} is not answered at message index {} of {records:?}",
                            entry.id,
                            index + 1 + offset
                        );
                        parents.push(entry.id.clone());
                    }
                    index += 1 + tool_calls.len();
                }
                MessageRecord::Tool { tool_call_id, .. } => {
                    assert!(
                        parents.iter().any(|id| id == tool_call_id),
                        "an orphan tool result {tool_call_id} survived normalization"
                    );
                    index += 1;
                }
                _ => index += 1,
            }
        }
    }

    // ------------------------------------------------------- dangling tool_calls
    // The live loop persists the assistant before dispatch and the abort break
    // never reaches the boundary repair, so the bundle's newest message is an
    // unanswered `tool_calls`.
    #[test]
    fn h1_dangling_tool_calls_are_answered_with_the_aborted_placeholder() {
        let messages = vec![
            sys("system prompt"),
            user("the goal"),
            asst(
                None,
                &[call("call-1", TOOL_READ_FILE, "{\"path\":\"src/main.rs\"}")],
            ),
        ];
        let (healed, report) = pass(messages.clone()).expect("dangling calls are repaired");

        assert_eq!(healed.len(), 4);
        assert_eq!(
            healed[2], messages[2],
            "the assistant turn is kept (option (a))"
        );
        assert_eq!(healed[3], tool("call-1", ABORTED_TOOL_RESULT));
        assert_sendable(&healed);

        assert_eq!(report.synthesized_aborted_ids, vec!["call-1".to_string()]);
        assert!(report.pruned_orphan_ids.is_empty());
        assert!(report.regrouped_ids.is_empty());
        assert_eq!(report.passes, PASS_SET_RUNS);
        assert!(report.stable);
        assert_eq!(report.messages_before, 3);
        assert_eq!(report.messages_after, 4);
        assert_eq!(report.pinned_prefix_end_before, 2);
        assert_eq!(report.pinned_prefix_end_after, 2);
        assert_eq!(
            report
                .applied
                .iter()
                .map(|rule| (rule.rule, rule.tool_call_id.as_str(), rule.index_after))
                .collect::<Vec<_>>(),
            vec![(AppliedRule::N2_SYNTHESIZE, "call-1", Some(3))]
        );

        // The sentence names the function and the verbatim arguments.
        let sentence = rule_sentence(&report.applied[0], "manager", &healed);
        assert!(sentence.contains(
            "synthesized (aborted) for tool_call_id call-1 (function read_file, manager)"
        ));
        assert!(sentence.contains("arguments {\"path\":\"src/main.rs\"}"));
    }

    // ------------------------------------------------------------ orphan results
    #[test]
    fn orphan_tool_results_are_pruned_and_reported_with_their_index() {
        let messages = vec![
            sys("system prompt"),
            user("the goal"),
            tool("call-gone", "output whose parent is gone"),
            asst(
                None,
                &[call("call-1", TOOL_RUN_COMMAND, "{\"command\":\"ls\"}")],
            ),
            tool("call-1", "listing"),
        ];
        let (healed, report) = pass(messages).expect("orphans are pruned");

        assert_eq!(report.pruned_orphan_ids, vec!["call-gone".to_string()]);
        assert_eq!(
            report
                .applied
                .iter()
                .map(|rule| (
                    rule.rule,
                    rule.tool_call_id.as_str(),
                    rule.index_before,
                    rule.index_after
                ))
                .collect::<Vec<_>>(),
            vec![
                (AppliedRule::N1_PRUNE, "call-gone", Some(2), None),
                // Regrouping is measured against the un-normalized vector, so a
                // surviving result that shifted down when the orphan in front of it
                // was pruned is reported with the bundle index.
                (AppliedRule::N2_REGROUP, "call-1", Some(4), Some(3)),
            ]
        );
        assert_eq!(report.regrouped_ids, vec!["call-1".to_string()]);
        assert_eq!(healed.len(), 4);
        assert_eq!(
            healed[2],
            asst(
                None,
                &[call("call-1", TOOL_RUN_COMMAND, "{\"command\":\"ls\"}")]
            )
        );
        assert_eq!(healed[3], tool("call-1", "listing"));
        assert_sendable(&healed);
        // The exact required wording for a pruned orphan.
        assert_eq!(
            rule_sentence(&report.applied[0], "manager", &healed),
            "pruned orphan tool result call-gone at message index 2 (manager)"
        );
    }

    // ------------------------------------------- one assistant, many calls, mixed
    #[test]
    fn one_assistant_with_many_calls_is_repaired_on_all_three_halves_at_once() {
        let four = vec![
            call("c1", TOOL_READ_FILE, "{\"path\":\"a.rs\"}"),
            call("c2", TOOL_REPLACE, "{\"path\":\"b.rs\"}"),
            call("c3", TOOL_GLOB, "{\"pattern\":\"**/*.rs\"}"),
            call("c4", TOOL_RUN_COMMAND, "{oops"),
        ];
        let messages = vec![
            sys("system prompt"),
            user("the goal"),
            asst(None, &four),
            tool("c3", "glob bytes"),
            tool("c1", "file bytes"),
            tool("c1", "duplicate result for the same id"),
            tool("c9", "orphan: no surviving parent"),
            asst(Some(""), &[]),
        ];
        let (healed, report) = pass(messages).expect("a mixed round is repaired");

        assert_sendable(&healed);
        assert_eq!(
            healed,
            vec![
                sys("system prompt"),
                user("the goal"),
                asst(None, &four),
                tool("c1", "file bytes"),
                tool("c2", ABORTED_TOOL_RESULT),
                tool("c3", "glob bytes"),
                tool("c4", ABORTED_TOOL_RESULT),
                // `Some("")` is a different fact from `None` and must survive.
                asst(Some(""), &[]),
            ],
            "results are regrouped under their parent in call order, first result kept"
        );
        assert_eq!(report.pruned_orphan_ids, vec!["c9".to_string()]);
        assert_eq!(
            report.synthesized_aborted_ids,
            vec!["c2".to_string(), "c4".to_string()]
        );
        assert_eq!(report.dropped_duplicate_ids, vec!["c1".to_string()]);
        assert_eq!(
            report.regrouped_ids,
            vec!["c3".to_string(), "c1".to_string()]
        );
        assert_eq!(report.messages_before, 8);
        assert_eq!(report.messages_after, 8);
        // Two messages gone (orphan + duplicate), two synthesized: the count alone
        // says nothing, which is why every rule is itemized in `applied`.
        assert_eq!(
            report
                .applied
                .iter()
                .map(|rule| rule.rule)
                .collect::<Vec<_>>(),
            vec![
                AppliedRule::N1_PRUNE,
                AppliedRule::N2_DEDUPLICATE,
                AppliedRule::N2_SYNTHESIZE,
                AppliedRule::N2_SYNTHESIZE,
                AppliedRule::N2_REGROUP,
                AppliedRule::N2_REGROUP,
            ]
        );
        // The duplicate is reported against the index it had in the bundle, and an
        // orphan is never also counted as a duplicate (which is why N1 runs first).
        assert_eq!(
            report
                .applied
                .iter()
                .map(|rule| (rule.rule, rule.tool_call_id.as_str(), rule.index_before))
                .collect::<Vec<_>>(),
            vec![
                (AppliedRule::N1_PRUNE, "c9", Some(6)),
                (AppliedRule::N2_DEDUPLICATE, "c1", Some(5)),
                (AppliedRule::N2_SYNTHESIZE, "c2", None),
                (AppliedRule::N2_SYNTHESIZE, "c4", None),
                (AppliedRule::N2_REGROUP, "c3", Some(3)),
                (AppliedRule::N2_REGROUP, "c1", Some(4)),
            ]
        );
        assert_eq!(
            applied_rules(&report),
            vec![
                "N1 pruned 1 orphan tool result(s): c9".to_string(),
                "N2 synthesized 2 (aborted) tool result(s): c2,c4".to_string(),
                "N2 regrouped 2 tool result(s): c3,c1".to_string(),
                "N2 dropped 1 duplicate tool result(s): c1".to_string(),
            ]
        );
    }

    // --------------------------------------------------- never touch the pins
    #[test]
    fn the_pinned_region_survives_the_pass_set_byte_for_byte() {
        let messages = vec![
            sys("system prompt"),
            user("the goal"),
            asst(None, &[call("c1", TOOL_READ_FILE, "{}")]),
            tool("c1", "file bytes"),
            checkpoint("compacted summary of the earlier turns"),
            asst(None, &[call("c2", TOOL_REPLACE, "{}")]),
        ];
        assert_eq!(pinned_prefix_end_records(&messages), 5);
        let (healed, report) = pass(messages.clone()).expect("repaired around the checkpoint");

        assert_eq!(
            &healed[..5],
            &messages[..5],
            "no pass may rewrite, drop or move a message inside the pinned window"
        );
        assert_eq!(
            healed[5], messages[5],
            "the dangling assistant turn is kept verbatim"
        );
        assert_eq!(healed[6], tool("c2", ABORTED_TOOL_RESULT));
        assert_eq!(report.pinned_prefix_end_before, 5);
        assert_eq!(report.pinned_prefix_end_after, 5);
        assert_eq!(witness_of(&healed), witness_of(&messages));
        assert_eq!(report.synthesized_aborted_ids, vec!["c2".to_string()]);
        assert!(
            report.regrouped_ids.is_empty(),
            "a result already under its parent does not move"
        );
        assert_sendable(&healed);
    }

    #[test]
    fn a_pass_that_would_move_the_pinned_window_is_a_pin_violation_not_a_repair() {
        // The orphan sits inside the pinned window, so pruning it would shift the
        // checkpoint — and the "the repair moved the window, accept it" branch is
        // forbidden.
        let messages = vec![
            sys("system prompt"),
            user("the goal"),
            tool("c-gone", "orphan inside the pinned window"),
            checkpoint("compacted summary"),
            asst(Some("done"), &[]),
        ];
        assert_eq!(pinned_prefix_end_records(&messages), 4);
        let refusal = pass(messages).expect_err("a moved window must be refused");
        assert_eq!(refusal.code, RefusalCode::PinViolation);
        assert!(
            refusal.message.contains("pinned_prefix_end_expected 4")
                && refusal.message.contains("pinned_prefix_end_actual 3"),
            "{}",
            refusal.message
        );
    }

    #[test]
    fn installed_pin_reverification_refuses_a_transcript_that_lost_its_identity() {
        let broken = section(vec![user("not a system prompt"), user("the goal")]);
        let refusal = verify_pins(&broken, PinStage::Installed)
            .expect_err("messages[0] must be the system prompt");
        assert_eq!(refusal.code, RefusalCode::PinViolation);
        assert_eq!(refusal.member.as_deref(), Some(MANAGER_FILE));
        assert!(
            refusal.message.contains("messages[0]"),
            "{}",
            refusal.message
        );

        let honest = section(vec![sys("system prompt"), user("the goal")]);
        assert!(verify_pins(&honest, PinStage::Installed).is_ok());
    }

    // ------------------------------------------------------------- N5 idempotence
    #[test]
    fn a_second_pass_set_over_a_healed_transcript_is_a_no_op() {
        let messages = vec![
            sys("system prompt"),
            user("the goal"),
            tool("c-gone", "orphan"),
            asst(
                None,
                &[
                    call("c1", TOOL_READ_FILE, "{}"),
                    call("c2", TOOL_GLOB, "{}"),
                ],
            ),
            tool("c2", "glob bytes"),
        ];
        let (healed, first) = pass(messages).expect("first pass set");
        assert_eq!(first.passes, PASS_SET_RUNS);
        assert!(first.stable);

        let (again, second) = pass(healed.clone()).expect("second pass set");
        assert_eq!(again, healed, "N5: the pass set must be a fixed point");
        assert!(second.applied.is_empty(), "{:?}", second.applied);
        assert!(second.pruned_orphan_ids.is_empty());
        assert!(second.synthesized_aborted_ids.is_empty());
        assert!(second.regrouped_ids.is_empty());
        assert_eq!(second.messages_before, second.messages_after);
        assert_eq!(second.passes, PASS_SET_RUNS);
        assert!(second.stable);
    }

    #[test]
    fn a_second_pass_that_is_not_a_no_op_is_refused_as_normalization_unstable() {
        let (_, report) = pass(vec![
            sys("system prompt"),
            user("the goal"),
            asst(None, &[call("c1", TOOL_READ_FILE, "{}")]),
        ])
        .expect("clean transcript");
        assert!(SecondPassEvidence::default().verdict(&report).is_none());

        let evidence = SecondPassEvidence {
            extra_pruned_ids: vec!["c1".to_string()],
            changed: true,
            installed_drift: Some(3),
        };
        let refusal = evidence
            .verdict(&report)
            .expect("non-idempotence is a verdict");
        assert_eq!(refusal.code, RefusalCode::NormalizationUnstable);
        assert_eq!(refusal.member.as_deref(), Some(MANAGER_FILE));
        for needle in [
            "the second N1 pass pruned 1 more tool result(s): c1",
            "the second `ensure_tool_call_pairs` pass changed the transcript",
            "differ at message 3",
            // Both pass reports are printed with their numbers.
            "pass 1 applied 1 rule(s) over 3 message(s)",
            "1 synthesized",
        ] {
            assert!(
                refusal.message.contains(needle),
                "missing {needle:?} in {}",
                refusal.message
            );
        }
    }

    // ------------------------------------------------------- N0/N4 byte exactness
    #[test]
    fn decode_drift_catches_none_folded_into_some_empty() {
        let stored = vec![
            sys("system prompt"),
            user("the goal"),
            MessageRecord::Assistant {
                content: None,
                reasoning_content: Some("thinking".to_string()),
                tool_calls: vec![call("c1", TOOL_READ_FILE, "{oops")],
            },
        ];
        let wire: Vec<crate::types::Message> = stored
            .iter()
            .cloned()
            .map(MessageRecord::into_wire)
            .collect();
        assert_eq!(decode_drift(&stored, &wire), None);

        // Exactly what `serialize_assistant_content` (`types/wire.rs`) does.
        let mut folded = wire.clone();
        if let crate::types::Message::Assistant { content, .. } = &mut folded[2] {
            *content = Some(String::new());
        }
        assert_eq!(decode_drift(&stored, &folded), Some(2));

        // Verbatim `arguments` are protected the same way.
        let mut rewritten = wire.clone();
        if let crate::types::Message::Assistant { tool_calls, .. } = &mut rewritten[2] {
            tool_calls[0].function.arguments = "{}".to_string();
        }
        assert_eq!(decode_drift(&stored, &rewritten), Some(2));

        assert_eq!(decode_drift(&stored, &wire[..2]), Some(2));
    }

    #[test]
    fn pinned_identity_drift_names_the_rewritten_pin() {
        let before = vec![sys("system prompt"), user("the goal"), tool("c1", "out")];
        assert_eq!(pinned_identity_drift(&before, &before, 2), None);

        let replaced_goal = vec![
            sys("system prompt"),
            user("a rewritten goal"),
            tool("c1", "out"),
        ];
        let detail = pinned_identity_drift(&before, &replaced_goal, 2)
            .expect("a replaced goal is a pin violation");
        assert!(
            detail.contains("messages[1] is no longer byte-identical"),
            "{detail}"
        );
        assert!(detail.contains("goal"), "{detail}");

        let re_roled = vec![user("system prompt"), user("the goal"), tool("c1", "out")];
        let detail = pinned_identity_drift(&before, &re_roled, 2)
            .expect("a re-roled pin is a pin violation");
        assert!(detail.contains("messages[0]"), "{detail}");

        // A checkpoint rewritten inside the window is caught too.
        let window = vec![sys("s"), user("g"), checkpoint("body")];
        let rewritten = vec![sys("s"), user("g"), checkpoint("rewritten body")];
        let detail = pinned_identity_drift(&window, &rewritten, 3)
            .expect("a rewritten checkpoint is a pin violation");
        assert!(detail.contains("pinned window"), "{detail}");
    }

    #[test]
    fn unpaired_message_drift_protects_advisory_and_checkpoint_turns() {
        let before = vec![
            sys("s"),
            user("g"),
            user("(CONTEXT BUDGET ADVISORY: 82%)"),
            asst(Some("ok"), &[]),
        ];
        assert_eq!(unpaired_message_drift(&before, &before), None);
        let dropped = vec![sys("s"), user("g"), asst(Some("ok"), &[])];
        let detail = unpaired_message_drift(&before, &dropped).expect("a dropped turn is drift");
        assert!(detail.contains("non-tool messages 4 -> 3"), "{detail}");
    }

    // --------------------------------------------------------------- N6 witness
    #[test]
    fn an_empty_or_short_history_has_no_pin_to_lose() {
        let no_pins = PinWitness {
            system_prompt_len: 0,
            goal_len: 0,
        };
        let mut engine = ContextEngine::new(64);
        let (healed, report) = normalize(
            Vec::new(),
            &DanglingToolCallState::Balanced,
            &no_pins,
            2,
            &mut engine,
        )
        .expect("a worker that never ran has an empty history, not a pin violation");
        assert!(healed.is_empty());
        assert_eq!(report.passes, PASS_SET_RUNS);
        assert!(report.stable);

        // A single unanswered call: repaired, and nothing claims it lost a pin.
        let one = vec![asst(None, &[call("c1", TOOL_READ_FILE, "{}")])];
        let (healed, report) = normalize(
            one.clone(),
            &DanglingToolCallState::observe(&one),
            &no_pins,
            2,
            &mut engine,
        )
        .expect("a one-turn transcript is repaired, not refused");
        assert_eq!(
            healed,
            vec![
                asst(None, &[call("c1", TOOL_READ_FILE, "{}")]),
                tool("c1", ABORTED_TOOL_RESULT)
            ]
        );
        assert_eq!(report.synthesized_aborted_ids, vec!["c1".to_string()]);

        // A worker mirror whose first two turns are not the manager's pinned pair
        // is left exactly as it is — the byte-identity claim, not a role rule.
        let odd = vec![user("brief"), user("answer")];
        let (healed, _) = normalize(
            odd.clone(),
            &DanglingToolCallState::Balanced,
            &no_pins,
            2,
            &mut engine,
        )
        .expect("a non-manager prefix is not a pin violation");
        assert_eq!(healed, odd);
    }

    #[test]
    fn n6_reports_a_witness_that_already_claimed_the_placeholder() {
        let messages = vec![
            sys("system prompt"),
            user("the goal"),
            asst(None, &[call("c1", TOOL_READ_FILE, "{}")]),
        ];
        let (healed, report) = pass_with(
            messages,
            DanglingToolCallState::Healed {
                aborted_tool_call_ids: vec!["c1".to_string()],
            },
        )
        .expect("a disagreement is reported, not refused");

        assert_eq!(report.n6_disagreements.len(), 1);
        assert!(
            report.n6_disagreements[0]
                .contains("witness and transcript disagree; reported, not reconciled"),
            "{}",
            report.n6_disagreements[0]
        );
        // Reported, never silently repaired: exactly one placeholder, still synthesized.
        assert_eq!(healed.len(), 4);
        assert_eq!(report.synthesized_aborted_ids, vec!["c1".to_string()]);
    }

    #[test]
    fn dispatched_dangling_calls_become_a_non_restorable_notice() {
        let mut report = RepairReport::default();
        let recorded = DanglingToolCallState::Dangling {
            calls: vec![DanglingCall {
                tool_call_id: "c1".to_string(),
                function_name: TOOL_RUN_COMMAND.to_string(),
                arguments: "{\"command\":\"rm -rf target\"}".to_string(),
                dispatched: true,
            }],
        };
        dispatched_dangling(&mut report, "manager", &recorded);
        assert_eq!(report.notices.len(), 1);
        assert_eq!(report.notices[0].kind, NonRestorableKind::PtyProcess);
        assert!(
            report.notices[0]
                .detail
                .contains("may have run and changed the workspace")
        );
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("was dispatched but never answered"));

        // Undispatched calls are a repair, not a lost capability.
        let mut report = RepairReport::default();
        dispatched_dangling(
            &mut report,
            "manager",
            &DanglingToolCallState::Dangling {
                calls: vec![DanglingCall {
                    tool_call_id: "c2".to_string(),
                    function_name: TOOL_READ_FILE.to_string(),
                    arguments: "{}".to_string(),
                    dispatched: false,
                }],
            },
        );
        assert!(report.notices.is_empty() && report.warnings.is_empty());
    }

    // ------------------------------------------------------ end to end, L0–L9
    #[test]
    fn load_bundle_repairs_loudly_and_never_rewrites_the_bundle() {
        let dir = tempfile::tempdir().expect("temp dir");
        let bundle = dir.path().join(".marmel").join("session");
        std::fs::create_dir_all(&bundle).expect("bundle dir");

        let manager = vec![
            sys("system prompt"),
            user("the goal"),
            asst(
                None,
                &[call(
                    "m1",
                    TOOL_RUN_COMMAND,
                    "{\"command\":\"cargo check\"}",
                )],
            ),
        ];
        let worker_history = vec![
            sys("specialist prompt"),
            user("specialist brief"),
            asst(None, &[call("w1", TOOL_READ_FILE, "{oops")]),
        ];
        let payload = CapturePayload {
            manager: section(manager),
            ui: Some(UiSection {
                status_line: "phase 2/3".to_string(),
                ..UiSection::default()
            }),
            workers: vec![worker_section("coder-t-008", worker_history)],
            identity_floors: IdentityFloors {
                worker_id_seq_floor: 3,
                completed_seq_floor: 1,
                notice_id_floor: 4,
            },
        };
        let policy = CommitPolicy {
            min_interval: std::time::Duration::from_millis(0),
            max_bundle_bytes: 32 * 1024 * 1024,
            allow_throttled: true,
        };
        write_bundle(
            &bundle,
            "session-1",
            &payload,
            ProcessEnding::InProgress,
            &policy,
            &CommitLock::new(),
        )
        .expect("commit must succeed");
        let before = std::fs::read(bundle.join(MANAGER_FILE)).expect("manager.json");

        let outcome = load_bundle(&bundle);
        let RestoreOutcome::Restored {
            session, report, ..
        } = outcome
        else {
            panic!("a dangling transcript must restore, got {outcome:?}");
        };

        // The pass set ran twice on both members and left a provider-valid vector.
        assert_eq!(report.normalization_passes, PASS_SET_RUNS);
        assert!(report.normalization_stable);
        assert!(report.pins_verified);
        assert_eq!(report.aborted_results_synthesized, 2);
        assert_eq!(report.orphan_tool_results_pruned, 0);
        assert_eq!(report.manager_messages_before, 3);
        assert_eq!(report.manager_messages_after, 4);
        assert_eq!(
            session.manager.messages,
            vec![
                sys("system prompt"),
                user("the goal"),
                asst(
                    None,
                    &[call(
                        "m1",
                        TOOL_RUN_COMMAND,
                        "{\"command\":\"cargo check\"}"
                    )]
                ),
                tool("m1", ABORTED_TOOL_RESULT),
            ]
        );
        // Per-worker before/after counts, machine-checkable.
        assert_eq!(
            report.worker_counts,
            vec![WorkerRepairCounts {
                key: "coder-t-008".to_string(),
                before: 3,
                after: 4,
                applied_rules: vec!["N2 synthesized 1 (aborted) tool result(s): w1".to_string()],
            }]
        );
        // Loud, and checkable by hand — one sentence per applied rule plus the
        // per-worker `history <before> -> <after>` line.
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("worker coder-t-008: history 3 -> 4"))
        );
        assert!(report.warnings.iter().any(|w| w.contains(
            "synthesized (aborted) for tool_call_id m1 (function run_command, manager)"
        )));
        assert!(report.warnings.iter().any(|w| w.contains(
            "synthesized (aborted) for tool_call_id w1 (function read_file, worker coder-t-008)"
        )));
        assert_sendable(&session.manager.messages);
        assert_sendable(&session.workers[0].history);

        // The loader is read-only with respect to disk.
        let after = std::fs::read(bundle.join(MANAGER_FILE)).expect("manager.json after");
        assert_eq!(
            before, after,
            "restore must not rewrite a byte of the bundle"
        );

        // Loading the untouched bundle again is the same verdict: the passes are
        // in-memory, so the second pass set is a no-op on the same stored bytes.
        let again = load_bundle(&bundle);
        let RestoreOutcome::Restored { report: second, .. } = again else {
            panic!("second load must restore, got {again:?}");
        };
        assert_eq!(report, second);
    }

    fn worker_section(key: &str, history: Vec<MessageRecord>) -> WorkerSection {
        WorkerSection {
            worker_key: key.to_string(),
            worker_id_seq_value: 2,
            role: Agent::Coder,
            task_id: Some("t-008".to_string()),
            status: WorkerStatus::InProgress,
            status_label: "running".to_string(),
            started: TimePoint::now(),
            completed: None,
            last_activity: None,
            worked_secs: 12.5,
            completed_seq: None,
            context_tokens: 64,
            last_tokens: 7,
            counters: Default::default(),
            brief: format!("brief for {key}"),
            snippets: vec!["snippet".to_string()],
            image_urls: vec![],
            audio_urls: vec![],
            recursion_granted: false,
            history: history.clone(),
            history_source: HistorySource::LiveMirror,
            dangling: DanglingToolCallState::observe(&history),
            unrecoverable_reason: None,
            non_restorable: vec![],
        }
    }
}
